use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
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
    /// which source served the last snapshot + freshness/validation info
    pub vpn_meta: RwLock<Option<crate::vpngate::FetchMeta>>,
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

/// Borrowed twin of [`StateFile`], used when writing: the on-disk shape is
/// identical, but the proxy pool is streamed straight out of the lock instead
/// of being deep-cloned into a throwaway `Vec` first.
struct StateFileRef<'a> {
    proxies: &'a HashMap<String, ProxyInfo>,
    vpn_tunnels: &'a Vec<VpnTunnel>,
    vpn_pool: &'a Vec<VpnServer>,
}

impl Serialize for StateFileRef<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut m = ser.serialize_map(Some(3))?;
        m.serialize_entry("proxies", &ProxySeq(self.proxies.values()))?;
        m.serialize_entry("vpn_tunnels", self.vpn_tunnels)?;
        m.serialize_entry("vpn_pool", self.vpn_pool)?;
        m.end()
    }
}

/// `Values` is not itself a `Serialize`, so emit it as a JSON array lazily.
struct ProxySeq<'a>(std::collections::hash_map::Values<'a, String, ProxyInfo>);

impl Serialize for ProxySeq<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(None)?;
        for p in self.0.clone() {
            seq.serialize_element(&p)?;
        }
        seq.end()
    }
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
            vpn_meta: RwLock::new(None),
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
        let tmp = self.data_dir.join("state.json.tmp");
        let json = {
            let map = self.proxies.read().await;
            let tunnels = self.vpn_tunnels.read().await;
            let pool = self.vpn_pool.read().await;
            let sf = StateFileRef {
                proxies: &map,
                vpn_tunnels: &tunnels,
                vpn_pool: &pool,
            };
            // serialise straight out of the guards: the pool can hold tens of
            // thousands of entries and must not be cloned before it is written
            let mut out: Vec<u8> = Vec::new();
            serde_json::to_writer(&mut out, &sf)?;
            out
        };
        tokio::fs::write(&tmp, json).await?;
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
            if cc.is_empty() || !cfg.filter.countries.iter().any(|c| c.to_uppercase() == cc) {
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
        // 手动模式：端口完全由用户在 UI 里勾选决定，自动分配不介入
        if !cfg.fanout.auto_assign {
            return;
        }
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
        // clamp in u64 first: `days as i64 * 86400` would wrap on an absurd
        // value, moving the cutoff into the future and pruning everything
        let days = days.min(MAX_PRUNE_DAYS);
        let cutoff = now_ts().saturating_sub((days as i64).saturating_mul(86400));
        let mut map = self.proxies.write().await;
        let before = map.len();
        map.retain(|_, p| p.alive || p.last_check.unwrap_or(0) > cutoff);
        before - map.len()
    }
}

/// Ceiling for [`AppState::prune`], see there.
const MAX_PRUNE_DAYS: u64 = 365_000;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TunnelStatus;

    const OLD_STATE: &str = r#"{
      "proxies": [{"key":"http://1.2.3.4:8080","protocol":"http","ip":"1.2.3.4","port":8080,"alive":true,"local_port":13000}],
      "vpn_tunnels": [{"server_key":"1.2.3.4|443","hostname":"h","local_port":13001,"status":"up","attempts":1}],
      "vpn_pool": [{"ip":"1.2.3.4","remote_port":443,"country_short":"JP"}]
    }"#;

    /// state.json written by an older build (plain strings) must still load.
    #[test]
    fn old_state_file_deserializes() {
        let sf: StateFile = serde_json::from_str(OLD_STATE).expect("old state file must parse");
        assert_eq!(sf.proxies.len(), 1);
        assert_eq!(sf.proxies[0].local_port, Some(13000));
        assert_eq!(sf.vpn_tunnels.len(), 1);
        assert_eq!(sf.vpn_tunnels[0].status, TunnelStatus::Up);
        // still readable by the historic literal comparisons
        assert!(sf.vpn_tunnels[0].status == "up");
        assert_eq!(sf.vpn_pool[0].country_short.as_deref(), Some("JP"));
    }

    /// and the statuses it wrote come back out unchanged, so the API keeps
    /// emitting the same strings.
    #[test]
    fn status_round_trips_through_the_same_strings() {
        for (raw, want) in [
            ("spawning", TunnelStatus::Spawning),
            ("up", TunnelStatus::Up),
            ("down", TunnelStatus::Down),
            ("failed", TunnelStatus::Failed),
            ("rotated", TunnelStatus::Rotated),
            ("blacklisted", TunnelStatus::Blacklisted),
        ] {
            let sf: StateFile = serde_json::from_str(&format!(
                r#"{{"vpn_tunnels":[{{"server_key":"k","hostname":"h","local_port":1,"status":"{raw}"}}]}}"#
            ))
            .expect("status must parse");
            assert_eq!(sf.vpn_tunnels[0].status, want);
            assert_eq!(
                serde_json::to_string(&sf.vpn_tunnels[0].status).unwrap(),
                format!("\"{raw}\"")
            );
        }
    }

    /// A status from another version must not fail the whole load, and must not
    /// compare equal to a real state (it is what keeps a bogus value out of the
    /// rotation logic).
    #[test]
    fn unknown_status_is_kept_but_matches_nothing() {
        let sf: StateFile = serde_json::from_str(
            r#"{"vpn_tunnels":[{"server_key":"k","hostname":"h","local_port":1,"status":"half-open"}]}"#,
        )
        .expect("unknown status must not break the file");
        let st = sf.vpn_tunnels[0].status;
        assert_eq!(st, TunnelStatus::Unknown);
        assert!(st != "up");
        assert!(st != "failed");
        assert!(st != "blacklisted");
    }

    /// A tunnel without a status falls back to the start of its lifecycle.
    #[test]
    fn missing_status_defaults_to_spawning() {
        let sf: StateFile = serde_json::from_str(
            r#"{"vpn_tunnels":[{"server_key":"k","hostname":"h","local_port":1}]}"#,
        )
        .expect("missing status must not break the file");
        assert_eq!(sf.vpn_tunnels[0].status, TunnelStatus::Spawning);
    }

    /// The streaming writer must produce byte-identical JSON to the owned
    /// struct it replaced: the file is re-read at startup and other tooling
    /// parses it.
    #[test]
    fn streaming_writer_keeps_the_on_disk_shape() {
        let mut map = HashMap::new();
        for key in ["http://1.2.3.4:8080", "socks5://5.6.7.8:1080"] {
            map.insert(
                key.to_string(),
                ProxyInfo {
                    key: key.to_string(),
                    ip: key
                        .rsplit_once("://")
                        .map(|(_, r)| r.split(':').next().unwrap_or("").to_string())
                        .unwrap_or_default(),
                    alive: true,
                    local_port: Some(13000),
                    ..Default::default()
                },
            );
        }
        let tunnels = vec![VpnTunnel {
            server_key: "1.2.3.4|443".into(),
            hostname: "h".into(),
            local_port: 13001,
            status: TunnelStatus::Blacklisted,
            ..Default::default()
        }];
        let pool = vec![VpnServer {
            ip: "1.2.3.4".into(),
            remote_port: 443,
            country_short: Some("JP".into()),
            ..Default::default()
        }];

        let owned = StateFile {
            proxies: map.values().cloned().collect(),
            vpn_tunnels: tunnels.clone(),
            vpn_pool: pool.clone(),
        };
        let streamed = StateFileRef {
            proxies: &map,
            vpn_tunnels: &tunnels,
            vpn_pool: &pool,
        };

        let want = serde_json::to_string(&owned).unwrap();
        let got = serde_json::to_string(&streamed).unwrap();
        assert_eq!(got, want);
        // and it round-trips back through the loader
        let back: StateFile = serde_json::from_str(&got).unwrap();
        assert_eq!(back.proxies.len(), 2);
        assert_eq!(back.vpn_tunnels[0].status, TunnelStatus::Blacklisted);
        assert_eq!(back.vpn_pool.len(), 1);
    }
}
