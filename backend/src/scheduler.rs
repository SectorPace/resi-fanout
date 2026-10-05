//! Background loops: periodic save, periodic source refresh, periodic recheck.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::models::now_ts;
use crate::sources;
use crate::state::AppState;

/// Releases `AppState::busy` when dropped, including while unwinding.
///
/// A cycle runs inside a detached `tokio::spawn`, so a panic anywhere in the
/// body kills only that task. With `busy` merely cleared on the straight-line
/// success path, such a panic left the flag set forever: every
/// `POST /api/refresh` and `POST /api/check` then answered 409, and the
/// periodic loop's `!state.busy.swap(true, ..)` never fired again — the
/// service stopped refreshing until an operator restarted it.
struct BusyGuard(Arc<AppState>);

impl BusyGuard {
    fn new(state: &Arc<AppState>) -> Self {
        Self(state.clone())
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

/// Minimum gap between state saves, and the ceiling the failure backoff grows
/// towards. The floor doubles on every consecutive failure and resets on the
/// first success, so a transient error costs one short delay while a permanent
/// one settles at `SAVE_CEILING` instead of retrying every 30 seconds forever.
const SAVE_FLOOR: Duration = Duration::from_secs(30);
const SAVE_CEILING: Duration = Duration::from_secs(3600);

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut last_save = std::time::Instant::now() - Duration::from_secs(60);
        // Backoff for a *persistently* failing save. `dirty` must stay set (see
        // below), so without this a read-only/full data_dir — or one the service
        // user lost write access to after the install-time chown — re-serialises
        // the entire multi-MB pool and retries every 30s forever, flooding the
        // journal with one warn per attempt.
        let mut save_fails: u32 = 0;
        let mut save_backoff = SAVE_FLOOR;
        loop {
            tick.tick().await;
            // a check round dirties the state constantly; with a few thousand
            // proxies every write is megabytes, so keep a floor between saves
            // (run_cycle still forces a save when it finishes).
            // `dirty` is only consumed once a save actually succeeded: clearing
            // it up front dropped the pending change whenever save_state()
            // failed (full disk, EACCES) with nothing left to retry from.
            if last_save.elapsed() >= save_backoff && state.dirty.load(Ordering::Relaxed)
            {
                match state.save_state().await {
                    Ok(()) => {
                        state.dirty.store(false, Ordering::Relaxed);
                        last_save = std::time::Instant::now();
                        save_fails = 0;
                        save_backoff = SAVE_FLOOR;
                    }
                    Err(e) => {
                        save_fails = save_fails.saturating_add(1);
                        save_backoff = (save_backoff * 2).min(SAVE_CEILING);
                        warn!(
                            error = %e,
                            consecutive_failures = save_fails,
                            next_retry_secs = save_backoff.as_secs(),
                            "state save failed; keeping dirty flag and backing off"
                        );
                    }
                }
            }
            let cfg = state.config().await;
            if cfg.scheduler.refresh_minutes == 0 {
                continue;
            }
            let now = now_ts();
            let last = *state.last_refresh.read().await;
            let next = *state.next_refresh.read().await;
            let due = match next {
                Some(t) => now >= t,
                None => match last {
                    Some(l) => now >= l + (cfg.scheduler.refresh_minutes as i64) * 60,
                    None => true, // first run: fetch immediately
                },
            };
            let recheck_due = now
                >= state
                    .last_check_all
                    .read()
                    .await
                    .map(|t| t + (cfg.scheduler.recheck_minutes as i64) * 60)
                    .unwrap_or(i64::MAX);
            if (due || recheck_due) && !state.busy.swap(true, Ordering::SeqCst) {
                let st = state.clone();
                tokio::spawn(async move {
                    run_cycle(&st).await;
                });
            }
        }
    });
}

/// One full cycle: prune → fetch sources → merge → health-check →
/// assign fanout ports → persist. Also used by POST /api/refresh.
pub async fn run_cycle(state: &Arc<AppState>) {
    // Claim nothing (the caller does that atomically) but guarantee release on
    // every exit path, including an unwind.
    let _busy = BusyGuard::new(state);
    let cfg = state.config().await;

    let pruned = state.prune(cfg.scheduler.prune_days).await;
    if pruned > 0 {
        info!(pruned, "pruned dead entries");
    }

    // 0) VPN Gate pool refresh (cheap CSV fetch) when enabled
    if cfg.vpngate.enabled {
        crate::vpngate::refresh_pool(state).await;
    }

    // 1) fetch all enabled sources concurrently
    let client = match sources::build_client() {
        Ok(c) => c,
        Err(e) => {
            // Not fatal: keep the existing pool and try again next cycle.
            warn!(error = %e, "cannot build http client, skipping source fetch");
            *state.source_status.write().await = cfg
                .sources
                .iter()
                .filter(|s| s.enabled)
                .map(|s| crate::models::SourceStatus {
                    name: s.name.clone(),
                    ok: false,
                    count: 0,
                    error: Some(format!("http client unavailable: {e}")),
                    ts: now_ts(),
                })
                .collect();
            state.assign_ports().await;
            let now = now_ts();
            *state.last_refresh.write().await = Some(now);
            *state.next_refresh.write().await =
                Some(now + (cfg.scheduler.refresh_minutes as i64) * 60);
            state.dirty.store(true, Ordering::Relaxed);
            let _ = state.save_state().await;
            return;
        }
    };
    let mut outcomes = sources::fetch_all(&client, &cfg.sources).await;

    let mut statuses = Vec::new();
    let mut added = 0usize;
    {
        let mut map = state.proxies.write().await;
        for out in &mut outcomes {
            statuses.push(crate::models::SourceStatus {
                name: out.name.clone(),
                ok: out.error.is_none(),
                count: out.proxies.len(),
                error: out.error.clone(),
                ts: now_ts(),
            });
            for mut p in out.proxies.drain(..) {
                p.source = out.name.clone();
                match map.get_mut(&p.key) {
                    None => {
                        map.insert(p.key.clone(), p);
                        added += 1;
                    }
                    Some(old) => {
                        // refresh source-side metadata, keep runtime state
                        if old.country.is_none() {
                            old.country = p.country.take();
                        }
                        if old.country_code.is_none() {
                            old.country_code = p.country_code.take();
                        }
                        if old.isp.is_none() {
                            old.isp = p.isp.take();
                        }
                        if old.anonymity.is_none() {
                            old.anonymity = p.anonymity.take();
                        }
                    }
                }
            }
        }
        // bound the pool: evict dead first (oldest check), then worst latency
        let max_pool = cfg.checker.max_pool.max(100);
        if map.len() > max_pool {
            let mut need = map.len() - max_pool;
            let mut dead: Vec<(String, i64)> = map
                .iter()
                .filter(|(_, p)| !p.alive)
                .map(|(k, p)| (k.clone(), p.last_check.unwrap_or(0)))
                .collect();
            dead.sort_by_key(|(_, t)| *t);
            for (k, _) in dead {
                if need == 0 {
                    break;
                }
                if map.remove(&k).is_some() {
                    need -= 1;
                }
            }
            if need > 0 {
                let mut alive: Vec<(String, u64)> = map
                    .iter()
                    .filter(|(_, p)| p.local_port.is_none())
                    .map(|(k, p)| (k.clone(), p.latency_ms.unwrap_or(u64::MAX)))
                    .collect();
                alive.sort_by(|a, b| b.1.cmp(&a.1));
                for (k, _) in alive {
                    if need == 0 {
                        break;
                    }
                    if map.remove(&k).is_some() {
                        need -= 1;
                    }
                }
            }
        }
    }
    *state.source_status.write().await = statuses;
    info!(added, "merged source results");

    // 2) health-check everything (new entries included)
    let keys: Vec<String> = state.proxies.read().await.keys().cloned().collect();
    crate::checker::check_all(state, keys).await;

    // 3) fan out ports, 4) persist bookkeeping
    state.assign_ports().await;
    let now = now_ts();
    *state.last_refresh.write().await = Some(now);
    *state.next_refresh.write().await =
        Some(now + (cfg.scheduler.refresh_minutes as i64) * 60);
    *state.last_check_all.write().await = Some(now);
    state.dirty.store(true, Ordering::Relaxed);
    let _ = state.save_state().await;
    // `busy` is released by BusyGuard on drop, so a panic above cannot wedge
    // the scheduler.
    info!("refresh cycle done");
}
