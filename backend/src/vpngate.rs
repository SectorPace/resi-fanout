//! VPN Gate public relay list (https://www.vpngate.net/api/iphone/).
//!
//! CSV layout (after two `#` comment lines):
//! HostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,
//! Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,OpenVPN_ConfigData_Base64
//! LogType/Operator/Message are Base64-encoded to survive CSV; the last
//! row starts with `*`.

use std::sync::Arc;

use base64::Engine as _;
use tracing::{info, warn};

use crate::models::VpnServer;
use crate::state::AppState;

fn b64_decode_maybe(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    String::from_utf8(bytes).ok()
}

pub fn parse_csv(text: &str) -> Vec<VpnServer> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') || line.starts_with('*') {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 15 {
            continue;
        }
        let config_b64 = f[14].to_string();
        let (remote_port, proto) =
            extract_remote(&b64_decode_maybe(&config_b64).unwrap_or_default());
        if remote_port == 0 {
            continue;
        }
        out.push(VpnServer {
            hostname: f[0].to_string(),
            ip: f[1].to_string(),
            score: f[2].parse().unwrap_or(0),
            ping_ms: f[3].parse().unwrap_or(0),
            speed_bps: f[4].parse().unwrap_or(0),
            country_long: nonempty(f[5]).map(String::from),
            country_short: nonempty(f[6]).map(String::from),
            sessions: f[7].parse().unwrap_or(0),
            uptime_secs: f[8].parse().unwrap_or(0),
            logs_kept: b64_decode_maybe(f[11]).map(|v| v != "False"),
            operator: b64_decode_maybe(f[12]),
            config_b64,
            remote_port,
            proto,
        });
    }
    out
}

fn nonempty(s: &str) -> Option<&str> {
    if s.is_empty() || s == "-" {
        None
    } else {
        Some(s)
    }
}

/// Pull the first `remote <host> <port>` and `proto <p>` out of the
/// embedded OpenVPN config.
fn extract_remote(config: &str) -> (u16, String) {
    let mut port = 0u16;
    let mut proto = "udp".to_string();
    for line in config.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("remote ") {
            let mut it = rest.split_whitespace();
            let _host = it.next();
            if let Some(p) = it.next().and_then(|p| p.parse().ok()) {
                if port == 0 {
                    port = p;
                }
            }
        } else if let Some(rest) = line.strip_prefix("proto ") {
            let p = rest.trim().to_ascii_lowercase();
            if p.starts_with("tcp") {
                proto = "tcp".into();
            } else if p.starts_with("udp") {
                proto = "udp".into();
            }
        }
    }
    (port, proto)
}

/// Fetch and parse the public list; replaces the pool in state.
pub async fn refresh_pool(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let client = crate::sources::build_client();
    let url = cfg.vpngate.api_url.clone();
    let fetch = async {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("vpngate api http {}", resp.status());
        }
        // vpngate.net declares charsets reqwest can't decode; take raw
        // bytes and decode leniently instead of trusting the header.
        let bytes = resp.bytes().await?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(parse_csv(&text))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(45), fetch).await {
        Ok(Ok(servers)) => {
            let n = servers.len();
            *state.vpn_pool.write().await = servers;
            state.vpn_pool_ts.store(crate::models::now_ts(), std::sync::atomic::Ordering::Relaxed);
            info!(count = n, "vpngate pool refreshed");
        }
        Ok(Err(e)) => warn!(error = %e, "vpngate pool fetch failed"),
        Err(_) => warn!("vpngate pool fetch timed out"),
    }
}

/// Apply the country / speed filters and rank candidates best-first.
pub fn rank(pool: &[VpnServer], countries: &[String], min_speed_mbps: u64) -> Vec<VpnServer> {
    let mut list: Vec<VpnServer> = pool
        .iter()
        .filter(|s| {
            if !countries.is_empty() {
                let cc = s.country_short.as_deref().unwrap_or("").to_uppercase();
                if cc.is_empty() || !countries.iter().any(|c| c.to_uppercase() == cc) {
                    return false;
                }
            }
            s.speed_bps >= min_speed_mbps * 1_000_000
        })
        .cloned()
        .collect();
    list.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.ping_ms.cmp(&b.ping_ms)));
    list
}
