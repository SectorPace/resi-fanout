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
            last_seen: 0,
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

/// Split a text blob of one or more OpenVPN configs (VPN Gate mirrors and
/// the like) into servers. Country is taken from the inline certificate's
/// `C=` when the source doesn't advertise one.
pub fn parse_ovpn_configs(text: &str) -> Vec<VpnServer> {
    let mut out: Vec<VpnServer> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in text.split("\n\n") {
        if !block.contains("remote ") {
            continue;
        }
        let (remote_port, proto) = extract_remote(block);
        if remote_port == 0 {
            continue;
        }
        let host = match block.lines().find_map(|l| {
            l.trim()
                .strip_prefix("remote ")
                .and_then(|r| r.split_whitespace().next().map(str::to_string))
        }) {
            Some(h) => h,
            None => continue,
        };
        let key = format!("{host}|{remote_port}");
        if !seen.insert(key.clone()) {
            continue;
        }
        let country = block
            .lines()
            .find_map(|l| {
                let t = l.trim();
                t.starts_with("subject=").then(|| t.to_string()).or_else(|| {
                    t.starts_with("/C=").then(|| t.to_string())
                })
            })
            .and_then(|s| cert_country(&s));
        out.push(VpnServer {
            hostname: host.clone(),
            ip: host,
            score: 0,
            ping_ms: 0,
            speed_bps: 0,
            country_long: None,
            country_short: country.map(|c| c.to_uppercase()),
            sessions: 0,
            uptime_secs: 0,
            logs_kept: None,
            operator: None,
            config_b64: base64::engine::general_purpose::STANDARD.encode(block.trim()),
            remote_port,
            proto,
            last_seen: 0,
        });
    }
    out
}

fn cert_country(subject: &str) -> Option<String> {
    // "/C=JP/O=.../CN=public-vpn-1" or "C = JP, O = ..."
    let idx = subject.find("/C=").or_else(|| subject.find("C = ").or_else(|| subject.find("C=")))?;
    let rest = &subject[idx + if subject[idx..].starts_with("/C=") { 3 } else { 2 }..];
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic())).then_some(code)
}

/// Fetch and parse the public list; merges into the persistent pool cache.
pub async fn refresh_pool(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let client = crate::sources::build_client();
    let url = cfg.vpngate.api_url.clone();
    let extras = cfg.vpngate.extra_urls.clone();
    let fetch = async {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("vpngate api http {}", resp.status());
        }
        // vpngate.net declares charsets reqwest can't decode; take raw
        // bytes and decode leniently instead of trusting the header.
        let bytes = resp.bytes().await?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        let mut servers = parse_csv(&text);

        for extra in &extras {
            match client.get(extra).send().await {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => {
                        let t = String::from_utf8_lossy(&b).to_string();
                        let n = parse_ovpn_configs(&t).len();
                        info!(url = %extra, count = n, "vpngate: extra ovpn source");
                        servers.extend(parse_ovpn_configs(&t));
                    }
                    Err(e) => warn!(url = %extra, error = %e, "vpngate: extra source body"),
                },
                Ok(r) => warn!(url = %extra, status = %r.status(), "vpngate: extra source"),
                Err(e) => warn!(url = %extra, error = %e, "vpngate: extra source"),
            }
        }
        Ok(servers)
    };

    match tokio::time::timeout(std::time::Duration::from_secs(60), fetch).await {
        Ok(Ok(fresh)) => {
            let now = crate::models::now_ts();
            let mut pool = state.vpn_pool.write().await;
            let live_before = fresh.len();
            // merge: refresh last_seen for live relays, keep cached ones so
            // nodes that went offline can be retried when they come back
            for mut s in fresh {
                s.last_seen = now;
                match pool.iter_mut().find(|p| p.server_key() == s.server_key()) {
                    Some(existing) => *existing = s,
                    None => pool.push(s),
                }
            }
            // prune by age + cap
            let cutoff = now - (cfg.vpngate.cache_days as i64) * 86400;
            pool.retain(|s| s.last_seen >= cutoff);
            if cfg.vpngate.max_pool > 0 && pool.len() > cfg.vpngate.max_pool {
                pool.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(b.score.cmp(&a.score)));
                pool.truncate(cfg.vpngate.max_pool);
            }
            let cached = pool.len();
            state.vpn_pool_ts.store(now, std::sync::atomic::Ordering::Relaxed);
            state.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            info!(live = live_before, cached, "vpngate pool refreshed");
        }
        Ok(Err(e)) => warn!(error = %e, "vpngate pool fetch failed"),
        Err(_) => warn!("vpngate pool fetch timed out"),
    }
}

/// Apply the country / speed filters and rank candidates best-first.
/// Servers seen recently are preferred over stale cached ones; unknown
/// speed (extra ovpn sources) is not penalised.
pub fn rank(pool: &[VpnServer], countries: &[String], min_speed_mbps: u64) -> Vec<VpnServer> {
    let now = crate::models::now_ts();
    let fresh_cutoff = now - 3600; // seen in the last hour = live
    let mut list: Vec<VpnServer> = pool
        .iter()
        .filter(|s| {
            if !countries.is_empty() {
                let cc = s.country_short.as_deref().unwrap_or("").to_uppercase();
                if cc.is_empty() || !countries.iter().any(|c| c.to_uppercase() == cc) {
                    return false;
                }
            }
            s.speed_bps == 0 || s.speed_bps >= min_speed_mbps * 1_000_000
        })
        .cloned()
        .collect();
    list.sort_by(|a, b| {
        let stale_a = (a.last_seen < fresh_cutoff) as u8;
        let stale_b = (b.last_seen < fresh_cutoff) as u8;
        stale_a
            .cmp(&stale_b)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.ping_ms.cmp(&b.ping_ms))
    });
    list
}
