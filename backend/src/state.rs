use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::config::Config;
use crate::models::{now_ts, ProxyInfo, SourceStatus, VpnServer, VpnTunnel};

pub struct AppState {
    pub cfg_path: String,
    pub data_dir: PathBuf,
    pub cfg: RwLock<Config>,
    pub proxies: RwLock<HashMap<String, ProxyInfo>>,
    pub source_status: RwLock<Vec<SourceStatus>>,
    pub last_refresh: RwLock<Option<i64>>,
    pub next_refresh: RwLock<Option<i64>>,
    pub last_check_all: RwLock<Option<i64>>,
    /// VPN Gate: latest parsed server pool + active tunnel assignments.
    pub vpn_pool: RwLock<Vec<VpnServer>>,
    pub vpn_pool_ts: std::sync::atomic::AtomicI64,
    pub vpn_tunnels: RwLock<Vec<VpnTunnel>>,
    pub busy: AtomicBool,
    pub dirty: AtomicBool,
    pub started_at: Instant,
}

#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    proxies: Vec<ProxyInfo>,
    #[serde(default)]
    vpn_tunnels: Vec<VpnTunnel>,
    #[serde(default)]
    vpn_pool: Vec<VpnServer>,
}

impl AppState {
    pub fn new(cfg_path: String, data_dir: PathBuf, cfg: Config) -> Self {
        Self {
            cfg_path,
            data_dir,
            cfg: RwLock::new(cfg),
            proxies: RwLock::new(HashMap::new()),
            source_status: RwLock::new(vec![]),
            last_refresh: RwLock::new(None),
            next_refresh: RwLock::new(None),
            last_check_all: RwLock::new(None),
            vpn_pool: RwLock::new(vec![]),
            vpn_pool_ts: std::sync::atomic::AtomicI64::new(0),
            vpn_tunnels: RwLock::new(vec![]),
            busy: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            started_at: Instant::now(),
        }
    }

    pub async fn config(&self) -> Config {
        self.cfg.read().await.clone()
    }

    pub async fn save_config(&self) -> anyhow::Result<()> {
        let cfg = self.cfg.read().await.clone();
        cfg.save_to(&self.cfg_path)
    }

    fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub async fn load_state(&self) {
        let path = self.state_file();
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            return;
        };
        match serde_json::from_str::<StateFile>(&text) {
            Ok(sf) => {
                let count;
                {
                    let mut map = self.proxies.write().await;
                    for p in sf.proxies {
                        map.insert(p.key.clone(), p);
                    }
                    count = map.len();
                }
                // tunnels reload with status reset: nothing is running yet
                let tunnels: Vec<VpnTunnel> = sf
                    .vpn_tunnels
                    .into_iter()
                    .map(|mut t| {
                        t.status = "spawning".into();
                        t.tun_ip = None;
                        t
                    })
                    .collect();
                *self.vpn_tunnels.write().await = tunnels;
                // cached VPN Gate relays survive restarts so nodes that were
                // offline can be retried when they come back
                *self.vpn_pool.write().await = sf.vpn_pool;
                tracing::info!(count, "state loaded");
            }
            Err(e) => warn!(error = %e, "failed to parse state file"),
        }
    }

    pub async fn save_state(&self) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.data_dir).await?;
        let map = self.proxies.read().await;
        let tunnels = self.vpn_tunnels.read().await;
        let pool = self.vpn_pool.read().await;
        let sf = StateFile {
            proxies: map.values().cloned().collect(),
            vpn_tunnels: tunnels.clone(),
            vpn_pool: pool.clone(),
        };
        drop(map);
        drop(tunnels);
        drop(pool);
        let tmp = self.data_dir.join("state.json.tmp");
        let data = serde_json::to_string(&sf)?;
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, self.state_file()).await?;
        Ok(())
    }

    /// True when the proxy should be fanned out (alive + user filter).
    pub fn passes_filter(cfg: &Config, p: &ProxyInfo) -> bool {
        if !p.alive {
            return false;
        }
        if cfg.filter.only_residential && !p.residential() {
            return false;
        }
        if !cfg.filter.countries.is_empty() {
            let cc = p.country_code.as_deref().unwrap_or("").to_uppercase();
            if cc.is_empty()
                || !cfg
                    .filter
                    .countries
                    .iter()
                    .any(|c| c.to_uppercase() == cc)
            {
                return false;
            }
        }
        if !cfg.filter.protocols.is_empty() {
            let ps = p.protocol.as_str();
            if !cfg
                .filter
                .protocols
                .iter()
                .any(|x| x.eq_ignore_ascii_case(ps))
            {
                return false;
            }
        }
        true
    }

    /// Give the best (alive + filtered) proxies one local port each.
    /// Lowest latency gets the lowest port; existing assignments are kept
    /// whenever still valid to avoid port churn.
    pub async fn assign_ports(&self) {
        let cfg = self.config().await;
        let max = cfg.fanout.max_ports as usize;
        if max == 0 {
            return;
        }
        let base = cfg.fanout.base_port as u32;
        let mut map = self.proxies.write().await;

        let mut scored: Vec<(String, u64)> = map
            .iter()
            .filter(|(_, p)| Self::passes_filter(&cfg, p))
            .map(|(k, p)| (k.clone(), p.latency_ms.unwrap_or(u64::MAX)))
            .collect();
        scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let chosen: Vec<String> = scored.into_iter().take(max).map(|(k, _)| k).collect();
        let chosen_set: HashSet<String> = chosen.iter().cloned().collect();

        for (k, p) in map.iter_mut() {
            if !chosen_set.contains(k) {
                p.local_port = None;
            }
        }

        let mut used: HashSet<u16> = HashSet::new();
        for k in &chosen {
            if let Some(p) = map.get(k) {
                if let Some(pt) = p.local_port {
                    used.insert(pt);
                }
            }
        }

        for k in &chosen {
            let Some(p) = map.get_mut(k) else { continue };
            if p.local_port.is_some() {
                continue;
            }
            if let Some(c) = next_free_port(&used, base, max) {
                used.insert(c);
                p.local_port = Some(c);
            }
        }
        drop(map);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Remove entries that have been dead for more than `days`.
    pub async fn prune(&self, days: u64) -> usize {
        let cutoff = now_ts() - (days as i64 * 86400);
        let mut map = self.proxies.write().await;
        let before = map.len();
        map.retain(|_, p| p.alive || p.last_check.unwrap_or(0) > cutoff);
        before - map.len()
    }
}

fn next_free_port(used: &HashSet<u16>, base: u32, max: usize) -> Option<u16> {
    for i in 0..max {
        let cand = base + i as u32;
        if cand > 65535 {
            return None;
        }
        let c = cand as u16;
        if !used.contains(&c) {
            return Some(c);
        }
    }
    None
}
