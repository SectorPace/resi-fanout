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
                t.starts_with("subject=")
                    .then(|| t.to_string())
                    .or_else(|| t.starts_with("/C=").then(|| t.to_string()))
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

/// Locate the `C=` marker of an X.509 subject and report how many bytes it
/// spans. The offset has to follow the pattern that actually matched: `"C = "`
/// is four characters wide while `"C="` is two, and getting that wrong turns
/// the documented `C = JP, O = ...` form into `"= JP..."`.
fn find_country_marker(subject: &str) -> Option<(usize, usize)> {
    for pat in ["/C=", "C = ", "C="] {
        if let Some(i) = subject.find(pat) {
            return Some((i, pat.len()));
        }
    }
    None
}

fn cert_country(subject: &str) -> Option<String> {
    // "/C=JP/O=.../CN=public-vpn-1" or "C = JP, O = ..."
    let (idx, width) = find_country_marker(subject)?;
    let rest = &subject[idx + width..];
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic())).then_some(code)
}

/// Metadata about the snapshot we last accepted — surfaced in the UI so you
/// can see how fresh the node list is and which source answered.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FetchMeta {
    pub source: String,
    pub rows: usize,
    pub bytes: usize,
    pub sha256: String,
    pub at: i64,
    pub errors: Vec<String>,
}

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_ROWS: usize = 5000;
/// Ceiling for the cache age, so a nonsense `cache_days` cannot wrap the
/// cutoff around and silently retain every relay forever.
const MAX_CACHE_DAYS: u64 = 365_000;
const CSV_MARKER: &str = "OpenVPN_ConfigData_Base64";

/// A captive portal or an error page must never make it into the pool, so a
/// response only counts when it carries the VPN Gate CSV header.
fn looks_like_vpngate_csv(text: &str) -> bool {
    text.contains(CSV_MARKER)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Ordered fetch chain: official HTTPS → official HTTP → mirrors → extra ovpn
/// sources. First response that validates wins.
async fn fetch_first_valid(
    client: &reqwest::Client,
    api_url: &str,
    mirrors: &[String],
    extras: &[String],
) -> Result<(Vec<VpnServer>, String, usize, String, String), Vec<String>> {
    let mut errors = Vec::new();

    let mut candidates: Vec<(String, bool)> = Vec::new(); // (url, is_ovpn_source)
    candidates.push((api_url.to_string(), false));
    if let Some(http_url) = api_url.strip_prefix("https://") {
        candidates.push((format!("http://{http_url}"), false));
    }
    for m in mirrors {
        candidates.push((m.clone(), false));
    }
    for e in extras {
        candidates.push((e.clone(), true));
    }

    for (url, ovpn_style) in candidates {
        let fetch_url = url.clone();
        let got: anyhow::Result<Vec<u8>> = async {
            let mut resp = client.get(&fetch_url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("http {}", resp.status());
            }
            // reject on the announced length first, then enforce the same cap
            // while streaming: buffering the body and only then comparing its
            // size leaves a hostile mirror free to OOM us.
            if let Some(len) = resp.content_length() {
                if len > MAX_SNAPSHOT_BYTES as u64 {
                    anyhow::bail!("too large: {len} bytes (cap {MAX_SNAPSHOT_BYTES})");
                }
            }
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if bytes.len() + chunk.len() > MAX_SNAPSHOT_BYTES {
                    anyhow::bail!("body exceeds the {MAX_SNAPSHOT_BYTES} byte cap");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        }
        .await;

        match got {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                let servers = if looks_like_vpngate_csv(&text) {
                    parse_csv(&text)
                } else if ovpn_style {
                    parse_ovpn_configs(&text)
                } else {
                    vec![]
                };
                if servers.is_empty() {
                    errors.push(format!("{url}: response did not validate"));
                    continue;
                }
                let servers: Vec<VpnServer> = servers.into_iter().take(MAX_SNAPSHOT_ROWS).collect();
                let size = bytes.len();
                let digest = sha256_hex(&bytes);
                return Ok((servers, url, size, digest, text));
            }
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    Err(errors)
}

fn snapshot_dir(state: &Arc<AppState>) -> std::path::PathBuf {
    state.data_dir.join("vpngate")
}

async fn save_snapshot(state: &Arc<AppState>, text: &str, meta: &FetchMeta) {
    let dir = snapshot_dir(state);
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(dir.join("snapshot.csv"), text).await;
    let _ = tokio::fs::write(
        dir.join("snapshot.json"),
        serde_json::to_string(meta).unwrap_or_default(),
    )
    .await;
}

/// Fetch and parse the public list; merges into the persistent pool cache.
/// On total failure the last known-good local snapshot is reused.
pub async fn refresh_pool(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let client = match crate::sources::build_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "vpngate: cannot build http client, keeping cached pool");
            return;
        }
    };

    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        fetch_first_valid(
            &client,
            &cfg.vpngate.api_url,
            &cfg.vpngate.mirror_urls,
            &cfg.vpngate.extra_urls,
        ),
    )
    .await;

    let (fresh, meta) = match fetched {
        Ok(Ok((servers, source, bytes, digest, text))) => {
            let rows = servers.len();
            let meta = FetchMeta {
                source,
                rows,
                bytes,
                sha256: digest,
                at: crate::models::now_ts(),
                errors: vec![],
            };
            save_snapshot(state, &text, &meta).await;
            (servers, meta)
        }
        Ok(Err(errors)) => {
            warn!(
                ?errors,
                "vpngate: all sources failed, falling back to local snapshot"
            );
            let dir = snapshot_dir(state);
            let recovered = tokio::fs::read_to_string(dir.join("snapshot.csv"))
                .await
                .ok();
            match recovered {
                Some(text) if looks_like_vpngate_csv(&text) => {
                    let servers = parse_csv(&text);
                    let meta = FetchMeta {
                        source: "local snapshot".into(),
                        rows: servers.len(),
                        bytes: text.len(),
                        sha256: sha256_hex(text.as_bytes()),
                        at: crate::models::now_ts(),
                        errors,
                    };
                    save_snapshot(state, &text, &meta).await;
                    (servers, meta)
                }
                _ => {
                    warn!("vpngate: no usable snapshot either, keeping the cached pool");
                    let mut src = state.vpn_meta.write().await;
                    if let Some(m) = src.as_mut() {
                        m.errors = errors;
                    }
                    return;
                }
            }
        }
        Err(_) => {
            warn!("vpngate: fetch timed out, keeping the cached pool");
            return;
        }
    };

    let now = meta.at;
    let mut pool = state.vpn_pool.write().await;
    let live_before = fresh.len();
    // merge: refresh last_seen for live relays, keep cached ones so nodes
    // that went offline can be retried when they come back
    for mut s in fresh {
        s.last_seen = now;
        match pool.iter_mut().find(|p| p.server_key() == s.server_key()) {
            Some(existing) => *existing = s,
            None => pool.push(s),
        }
    }
    // prune by age + cap
    let days = cfg.vpngate.cache_days.min(MAX_CACHE_DAYS);
    let cutoff = now.saturating_sub((days as i64).saturating_mul(86400));
    pool.retain(|s| s.last_seen >= cutoff);
    if cfg.vpngate.max_pool > 0 && pool.len() > cfg.vpngate.max_pool {
        pool.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(b.score.cmp(&a.score)));
        pool.truncate(cfg.vpngate.max_pool);
    }
    let cached = pool.len();
    drop(pool);
    state
        .vpn_pool_ts
        .store(now, std::sync::atomic::Ordering::Relaxed);
    *state.vpn_meta.write().await = Some(meta.clone());
    state
        .dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
    info!(source = %meta.source, live = live_before, cached, sha = %&meta.sha256[..8.min(meta.sha256.len())], "vpngate pool refreshed");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_country_parses_all_documented_shapes() {
        // openssl-style one-line subject
        assert_eq!(cert_country("/C=JP/O=x/CN=y").as_deref(), Some("JP"));
        // the "C = JP, O = ..." form printed by `openssl x509 -subject`
        assert_eq!(cert_country("C = JP, O = x").as_deref(), Some("JP"));
        // bare marker, as embedded in an .ovpn `subject=` line
        assert_eq!(cert_country("C=JP").as_deref(), Some("JP"));
    }

    #[test]
    fn cert_country_rejects_unusable_values() {
        // no country marker at all
        assert_eq!(cert_country("CN=public-vpn-1"), None);
        // three-letter code is not an ISO country
        assert_eq!(cert_country("/C=JPN/O=x"), None);
        // digits are not a country code
        assert_eq!(cert_country("C = 12, O = x"), None);
        // empty value
        assert_eq!(cert_country("/C=/O=x"), None);
    }
}
