//! Cloudflare WARP as a first-class exit source.
//!
//! Two ways in:
//!   * `wgcf register` (official tool, handles WARP+ licenses and
//!     Cloudflare's TLS fingerprinting) when no profile exists yet;
//!   * pasting an existing WireGuard config (wgcf profile or hand-written).
//!
//! The profile is managed with wg-quick using `Table = off` so the host
//! routing table is untouched; our own source policy route (installed by
//! warp-up.sh) sends only sockets bound to the tunnel IP through the
//! tunnel. The tunnel then exposes a local SOCKS port exactly like the
//! VPN Gate relays, so it flows into /api/ports and 3x-ui inbound linking.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::process::Command;
use tracing::{info, warn};

use crate::models::now_ts;
use crate::relay::{self, Dialer};
use crate::state::AppState;

#[derive(Clone, Debug, Serialize, Default)]
pub struct WgProfile {
    pub private_key: String,
    pub addresses: Vec<String>,
    pub dns: Vec<String>,
    pub public_key: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
    pub keepalive: u64,
    pub mtu: u64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct WarpStatus {
    pub enabled: bool,
    pub profile_present: bool,
    pub tools: ToolStatus,
    pub up: bool,
    pub tun_ip: Option<String>,
    pub local_port: u16,
    pub exit_ip: Option<String>,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub isp: Option<String>,
    pub latency_ms: Option<u64>,
    pub hosting: Option<bool>,
    pub last_check: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct ToolStatus {
    pub wg_quick: bool,
    pub wg: bool,
    pub wgcf: bool,
}

pub fn parse_profile(text: &str) -> WgProfile {
    let mut p = WgProfile::default();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_lowercase();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_lowercase(), v.trim());
        match (section.as_str(), k.as_str()) {
            ("interface", "privatekey") => p.private_key = v.to_string(),
            ("interface", "address") => p.addresses.push(v.to_string()),
            ("interface", "dns") => p.dns.extend(v.split(',').map(|x| x.trim().to_string())),
            ("interface", "mtu") => p.mtu = v.parse().unwrap_or(0),
            ("peer", "publickey") => p.public_key = v.to_string(),
            ("peer", "endpoint") => p.endpoint = v.to_string(),
            ("peer", "allowedips") => {
                p.allowed_ips.extend(v.split(',').map(|x| x.trim().to_string()))
            }
            ("peer", "persistentkeepalive") => p.keepalive = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

/// Build the config wg-quick actually runs: DNS removed (we resolve through
/// the host), `Table = off` + our policy-routing hooks.
fn managed_conf(
    profile: &WgProfile,
    iface: &str,
    scripts_dir: &str,
    keepalive: u64,
    mtu: u64,
    table: u16,
) -> String {
    let up = Path::new(scripts_dir).join("warp-up.sh");
    let down = Path::new(scripts_dir).join("warp-down.sh");
    let ka = if keepalive > 0 { keepalive } else { profile.keepalive };
    let m = if mtu > 0 { mtu } else { profile.mtu };
    let mut ips = profile.addresses.clone();
    if ips.is_empty() {
        ips.push("172.16.0.2/32".into());
    }
    format!(
        "[Interface]\nPrivateKey = {}\nAddress = {}\nMTU = {}\nTable = off\nEnvironment = WARP_TABLE={}\nPostUp = {} up {}\nPostDown = {} down {}\n\n[Peer]\nPublicKey = {}\nEndpoint = {}\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = {}\n",
        profile.private_key,
        ips.join(", "),
        m,
        table,
        up.display(),
        iface,
        down.display(),
        iface,
        profile.public_key,
        if profile.endpoint.is_empty() { "engage.cloudflareclient.com:2408" } else { &profile.endpoint },
        ka.max(15),
    )
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.exists().then_some(p);
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.join(bin))
        .find(|p| p.exists())
}

pub fn tools() -> ToolStatus {
    ToolStatus {
        wg_quick: which("wg-quick").is_some(),
        wg: which("wg").is_some(),
        wgcf: which("wgcf").is_some(),
    }
}

async fn read_profile(state: &Arc<AppState>) -> Option<WgProfile> {
    let cfg = state.config().await;
    let text = tokio::fs::read_to_string(&cfg.warp.conf_path).await.ok()?;
    Some(parse_profile(&text))
}

pub async fn status(state: &Arc<AppState>) -> WarpStatus {
    let cfg = state.config().await;
    let profile = read_profile(state).await;
    let tools = tools();
    let up = tools.wg_quick && interface_exists(&cfg.warp.interface).await;
    let mut st = WarpStatus {
        enabled: cfg.warp.enabled,
        profile_present: profile.is_some(),
        tools,
        up,
        local_port: cfg.warp.local_port,
        ..Default::default()
    };
    // classification cache written by the supervisor
    if let Ok(text) = tokio::fs::read_to_string(
        state.data_dir.join("warp").join("status.json"),
    )
    .await
    {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            st.tun_ip = v.get("tun_ip").and_then(|x| x.as_str()).map(String::from);
            st.exit_ip = v.get("exit_ip").and_then(|x| x.as_str()).map(String::from);
            st.country = v.get("country").and_then(|x| x.as_str()).map(String::from);
            st.country_code = v.get("country_code").and_then(|x| x.as_str()).map(String::from);
            st.isp = v.get("isp").and_then(|x| x.as_str()).map(String::from);
            st.latency_ms = v.get("latency_ms").and_then(|x| x.as_u64());
            st.hosting = v.get("hosting").and_then(|x| x.as_bool());
            st.last_check = v.get("last_check").and_then(|x| x.as_i64());
            st.error = v.get("error").and_then(|x| x.as_str()).map(String::from);
        }
    }
    st
}

async fn interface_exists(iface: &str) -> bool {
    Path::new(&format!("/sys/class/net/{iface}")).exists()
}

async fn write_status(state: &Arc<AppState>, v: &Value) {
    let dir = state.data_dir.join("warp");
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(dir.join("status.json"), v.to_string()).await;
}

/// `wgcf register` (with license when configured), then adopt the profile.
pub async fn register(state: &Arc<AppState>, license: Option<String>) -> Result<String, String> {
    let cfg = state.config().await;
    if which("wgcf").is_none() {
        return Err("wgcf not installed — https://github.com/ViRb3/wgcf/releases (or paste a WireGuard config instead)".into());
    }
    let lic = license.or_else(|| {
        if cfg.warp.license.is_empty() {
            None
        } else {
            Some(cfg.warp.license.clone())
        }
    });
    let mut cmd = Command::new("wgcf");
    cmd.arg("register");
    if let Some(l) = &lic {
        cmd.arg("--license").arg(l);
    }
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.output(),
    )
    .await
    .map_err(|_| "wgcf register timed out".to_string())?
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "wgcf register failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let src = PathBuf::from(home).join(".wgcf/wgcf-profile.conf");
    let text = tokio::fs::read_to_string(&src)
        .await
        .map_err(|e| format!("profile not found after wgcf: {e}"))?;
    import_profile(state, &text).await?;
    Ok(if lic.is_some() { "WARP+ profile registered" } else { "WARP profile registered" }.into())
}

/// Save a pasted WireGuard config as our profile.
pub async fn import_profile(state: &Arc<AppState>, text: &str) -> Result<WgProfile, String> {
    let p = parse_profile(text);
    if p.private_key.is_empty() || p.public_key.is_empty() {
        return Err("config needs [Interface] PrivateKey and [Peer] PublicKey".into());
    }
    let cfg = state.config().await;
    let path = PathBuf::from(&cfg.warp.conf_path);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&path, text).await.map_err(|e| e.to_string())?;
    info!(path = %cfg.warp.conf_path, endpoint = %p.endpoint, "warp profile imported");
    Ok(p)
}

pub async fn connect(state: &Arc<AppState>) -> Result<(), String> {
    let cfg = state.config().await;
    let t = tools();
    if !t.wg_quick {
        return Err("wg-quick not found — install wireguard-tools".into());
    }
    let Some(profile) = read_profile(state).await else {
        return Err("no WireGuard profile — register with wgcf or paste a config".into());
    };
    let dir = PathBuf::from(&cfg.warp.conf_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let managed = dir.join(format!("{}.managed.conf", cfg.warp.interface));
    let conf = managed_conf(
        &profile,
        &cfg.warp.interface,
        &cfg.vpngate.scripts_dir,
        cfg.warp.keepalive,
        cfg.warp.mtu,
        cfg.warp.local_port,
    );
    tokio::fs::write(&managed, conf).await.map_err(|e| e.to_string())?;

    // tear down a stale interface first
    let _ = tokio::process::Command::new("wg-quick")
        .args(["down", &cfg.warp.interface])
        .output()
        .await;

    let out = tokio::process::Command::new("wg-quick")
        .args(["up", &managed.to_string_lossy()])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        write_status(
            state,
            &json!({ "error": format!("wg-quick up failed: {err}"), "last_check": now_ts() }),
        )
        .await;
        return Err(format!("wg-quick up failed: {err}"));
    }
    info!(interface = %cfg.warp.interface, port = cfg.warp.local_port, "warp tunnel up");
    Ok(())
}

pub async fn disconnect(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let _ = tokio::process::Command::new("wg-quick")
        .args(["down", &cfg.warp.interface])
        .output()
        .await;
    info!("warp tunnel down");
}

/// One-shot: bring the tunnel up if needed, publish the SOCKS port,
/// classify the exit, and persist status for the UI.
pub async fn run_cycle(state: &Arc<AppState>) {
    let cfg = state.config().await;
    if !cfg.warp.enabled {
        return;
    }
    if !tools().wg_quick {
        write_status(state, &json!({ "error": "wg-quick not installed", "last_check": now_ts() })).await;
        return;
    }
    if !interface_exists(&cfg.warp.interface).await {
        if let Err(e) = connect(state).await {
            warn!(error = %e, "warp connect failed");
            return;
        }
    }
    // tunnel IP = first address of the interface
    let tun_ip = tun_ip_of(&cfg.warp.interface).await;
    let Some(ip) = tun_ip else {
        write_status(state, &json!({ "error": "interface has no address", "last_check": now_ts() })).await;
        return;
    };

    // start the fanout listener once the tunnel is up
    ensure_listener(state, ip).await;

    let classified = crate::checker::classify_from(ip, &cfg).await;
    let empty = String::new();
    let (exit_ip, country, cc, isp, hosting, latency) = match classified {
        Some((e, l)) => (e.ip, e.country, e.country_code, e.isp, e.hosting, Some(l)),
        None => (empty.clone(), None, None, None, None, None),
    };
    write_status(
        state,
        &json!({
            "tun_ip": ip.to_string(),
            "exit_ip": exit_ip,
            "country": country, "country_code": cc, "isp": isp,
            "hosting": hosting, "latency_ms": latency,
            "last_check": now_ts(),
            "error": if exit_ip.is_empty() { json!("exit check failed") } else { serde_json::Value::Null },
        }),
    )
    .await;
    add_tunnel_entry(state, cfg.warp.local_port, "warp", "cloudflare-warp").await;
}

async fn tun_ip_of(iface: &str) -> Option<std::net::IpAddr> {
    let out = tokio::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.split_whitespace().collect();
    let idx = parts.iter().position(|p| *p == "inet")?;
    parts.get(idx + 1)?.split('/').next()?.parse().ok()
}

async fn add_tunnel_entry(state: &Arc<AppState>, port: u16, key: &str, hostname: &str) {
    // reuse the tunnel list so /api/ports and 3x-ui linking pick it up
    let mut tunnels = state.vpn_tunnels.write().await;
    if let Some(t) = tunnels.iter_mut().find(|t| t.local_port == port) {
        t.status = "up".into();
    } else {
        tunnels.push(crate::models::VpnTunnel {
            server_key: key.into(),
            hostname: hostname.into(),
            local_port: port,
            status: "up".into(),
            attempts: 0,
            alive: true,
            last_check: Some(now_ts()),
            ..Default::default()
        });
    }
}

/// strip local port entries when disabled
pub async fn clear_ports(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let mut tunnels = state.vpn_tunnels.write().await;
    tunnels.retain(|t| {
        !((t.server_key == "warp" && t.local_port == cfg.warp.local_port)
            || (t.server_key == "masque" && t.local_port == cfg.warp.mihomo_port))
    });
}

/// idempotent listener for the tunnel's SOCKS port
async fn ensure_listener(state: &Arc<AppState>, ip: std::net::IpAddr) {
    static LISTENING: std::sync::OnceLock<std::sync::Mutex<Vec<(u16, std::net::IpAddr)>>> =
        std::sync::OnceLock::new();
    let cfg = state.config().await;
    let port = cfg.warp.local_port;
    let set = LISTENING.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    {
        let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
        if guard.iter().any(|(p, i)| *p == port && *i == ip) {
            return;
        }
        guard.retain(|(p, i)| *p != port || *i == ip);
        guard.push((port, ip));
    }
    let st = state.clone();
    tokio::spawn(async move {
        let _ = relay::run_listener(st, port, Dialer::Tun(ip)).await;
    });
    info!(port, %ip, "warp fanout port listening");
}

/// Xray-native wireguard outbound built from the stored profile.
pub async fn xray_outbound(state: &Arc<AppState>) -> Result<Value, String> {
    let cfg = state.config().await;
    let Some(p) = read_profile(state).await else {
        return Err("no WireGuard profile — register with wgcf or paste a config".into());
    };
    let mut ips: Vec<String> = p.addresses.clone();
    if ips.is_empty() {
        ips = vec!["172.16.0.2/32".to_string()];
    }
    Ok(json!({
        "tag": "warp",
        "protocol": "wireguard",
        "settings": {
            "secretKey": p.private_key,
            "address": ips,
            "mtu": if cfg.warp.mtu > 0 { cfg.warp.mtu } else { 1280 },
            "peers": [{
                "publicKey": p.public_key,
                "endpoint": if p.endpoint.is_empty() { "engage.cloudflareclient.com:2408".to_string() } else { p.endpoint.clone() },
                "allowedIPs": ["0.0.0.0/0", "::/0"],
                "keepAlive": if cfg.warp.keepalive > 0 { cfg.warp.keepalive } else { 60 },
            }]
        },
        "streamSettings": { "network": "tcp" }
    }))
}

/// Apply a Clash/Mihomo node in one of two ways:
/// * "wireguard" — save the credentials as our WireGuard profile (wg-quick);
/// * "masque"    — write a self-contained Mihomo config and let the sidecar
///                  expose a local socks port (native MASQUE support).
pub async fn apply_clash(
    state: &Arc<AppState>,
    yaml: &str,
    index: usize,
    mode: &str,
) -> Result<Value, String> {
    let nodes = parse_clash(yaml);
    let Some(node) = nodes.get(index) else {
        return Err(format!("no masque/wireguard node at index {index}"));
    };
    let cfg = state.config().await;
    match mode {
        "masque" => {
            let conf = clash_to_mihomo(node, cfg.warp.mihomo_port);
            let path = PathBuf::from(&cfg.warp.mihomo_conf);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
            }
            tokio::fs::write(&path, conf).await.map_err(|e| e.to_string())?;
            // restart sidecar with the new node
            let taken = {
                MASQUE_SLOT
                    .get_or_init(|| std::sync::Mutex::new(None))
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            };
            if let Some(mut c) = taken {
                let _ = c.kill().await;
            }
            let started = match start_masque(&cfg, state.clone()).await {
                Ok(()) => true,
                Err(e) => {
                    warn!(error = %e, "mihomo sidecar not started");
                    false
                }
            };
            Ok(json!({
                "ok": true, "mode": "masque", "node": node.name,
                "conf": cfg.warp.mihomo_conf, "port": cfg.warp.mihomo_port,
                "sidecar_started": started,
                "hint": if started { String::new() } else { format!("run: {} -f {}", cfg.warp.mihomo_bin, cfg.warp.mihomo_conf) }
            }))
        }
        _ => {
            let conf = clash_to_wireguard(node, cfg.warp.keepalive, cfg.warp.mtu)?;
            let p = import_profile(state, &conf).await?;
            Ok(json!({
                "ok": true, "mode": "wireguard", "node": node.name,
                "endpoint": p.endpoint, "addresses": p.addresses,
                "hint": "saved as the WARP profile — press 连接 to bring the tunnel up"
            }))
        }
    }
}

// ------------------------------------------------------------ Clash / Mihomo import

/// A `masque` / `wireguard` node lifted out of a Clash or Mihomo config.
#[derive(Clone, Debug, Serialize, Default)]
pub struct ClashNode {
    pub name: String,
    pub kind: String,
    pub server: String,
    pub port: u16,
    /// verbatim values from the yaml — Mihomo needs these exactly as-is
    pub private_key: String,
    pub public_key: String,
    /// plain 32-byte keys, only when the blob is unambiguous
    pub wg_private_key: Option<String>,
    pub wg_public_key: Option<String>,
    pub ip: Option<String>,
    pub ipv6: Option<String>,
    pub mtu: Option<u64>,
    pub sni: Option<String>,
    pub dns: Vec<String>,
    pub udp: bool,
}

impl ClashNode {
    pub fn addresses(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(ip) = &self.ip {
            if !ip.is_empty() {
                v.push(ip.clone());
            }
        }
        if let Some(ip6) = &self.ipv6 {
            if !ip6.is_empty() {
                v.push(ip6.clone());
            }
        }
        v
    }
}

fn copy32(src: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(&src[..32]);
    k
}

/// Non-recursive DER TLV scan: collect every 32-byte key chunk we can find
/// (raw OCTET STRINGs, and BIT STRINGs that carry no unused bits). Clash
/// wraps WARP keys in PKCS#8 / SPKI DER, plain WireGuard configs use the
/// raw 32-byte form.
fn der_candidates(buf: &[u8]) -> Vec<[u8; 32]> {
    let mut out: Vec<[u8; 32]> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = vec![(0, buf.len())];
    while let Some((start, end)) = ranges.pop() {
        if start >= end {
            continue;
        }
        let mut i = start;
        while i + 2 <= end {
            let tag = buf[i];
            let first = buf[i + 1];
            let mut p = i + 2;
            let len: usize;
            if first & 0x80 != 0 {
                let n = (first & 0x7f) as usize;
                if n == 0 || n > 4 || p + n > end {
                    break;
                }
                let mut v: usize = 0;
                for k in 0..n {
                    v = (v << 8) | buf[p + k] as usize;
                }
                p += n;
                len = v;
            } else {
                len = first as usize;
            }
            if len > end - p {
                break;
            }
            if len == 32 {
                out.push(copy32(&buf[p..p + 32]));
            } else if len == 33 && buf[p] == 0 {
                out.push(copy32(&buf[p + 1..p + 33]));
            }
            if tag & 0x20 != 0 {
                ranges.push((p, p + len));
            }
            i = p + len;
        }
    }
    out
}

pub fn normalize_key(b64: &str) -> Option<String> {
    use base64::Engine as _;
    let trimmed = b64.trim();
    let raw = match base64::engine::general_purpose::STANDARD.decode(trimmed) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, len = trimmed.len(), head = %trimmed.chars().take(24).collect::<String>(), "key base64 decode failed");
            return None;
        }
    };
    if raw.len() == 32 {
        return Some(trimmed.to_string());
    }
    let cands = der_candidates(&raw);
    if cands.is_empty() {
        warn!(raw_len = raw.len(), head = hex_head(&raw), "no 32-byte chunk found in key blob");
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(cands[0]))
}

fn hex_head(raw: &[u8]) -> String {
    raw.iter().take(20).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("")
}

/// Extract the top-level `proxies:` block as text. Clash configs use YAML
/// anchors/merge keys (`<<: *domain`) further down, which serde_yaml rejects
/// outright — so we never try to parse the whole document.
fn extract_proxies_block(yaml: &str) -> Option<String> {
    let mut lines = yaml.lines();
    lines.position(|l| {
        let t = l.trim_end();
        t == "proxies:" || t.starts_with("proxies: ")
    })?;
    let mut body: Vec<&str> = Vec::new();
    for line in lines {
        // a new top-level key ends the block
        let t = line.trim_end();
        if !t.is_empty() && !t.starts_with(' ') && !t.starts_with('\t') && t.contains(':') {
            break;
        }
        body.push(line);
    }
    if body.is_empty() {
        return None;
    }
    Some(body.join("\n"))
}

/// Extract every masque / wireguard node from a Clash or Mihomo YAML.
pub fn parse_clash(yaml: &str) -> Vec<ClashNode> {
    let block = extract_proxies_block(yaml);
    let doc: serde_yaml::Value = match block.as_deref().map(serde_yaml::from_str) {
        Some(Ok(v)) => v,
        _ => match serde_yaml::from_str(yaml) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "clash yaml parse failed");
                return vec![];
            }
        },
    };
    let seq = doc
        .as_sequence()
        .map(|s| s.clone())
        .or_else(|| doc.get("proxies").and_then(|p| p.as_sequence()).cloned())
        .unwrap_or_default();
    let proxies = seq;
    let s = |m: &serde_yaml::Mapping, k: &str| -> Option<String> {
        m.get(serde_yaml::Value::String(k.into()))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    // Clash writes `port: 443` / `mtu: 1280` as numbers, not strings
    let num = |m: &serde_yaml::Mapping, k: &str| -> Option<u64> {
        let v = m.get(serde_yaml::Value::String(k.into()))?;
        v.as_u64().or_else(|| v.as_str().and_then(|x| x.parse().ok()))
    };
    let mut out = Vec::new();
    for p in proxies {
        let Some(m) = p.as_mapping() else { continue };
        let kind = s(m, "type").unwrap_or_default().to_lowercase();
        if kind != "masque" && kind != "wireguard" {
            continue;
        }
        let Some(priv_raw) = s(m, "private-key") else { continue };
        let Some(pub_raw) = s(m, "public-key") else { continue };
        // MASQUE keys are Cloudflare multi-algorithm containers (X25519+X448)
        // that only Mihomo can consume; keep them verbatim and mark whether a
        // plain-wireguard extraction was unambiguous.
        let wg_private_key = normalize_key(&priv_raw);
        let wg_public_key = normalize_key(&pub_raw);
        out.push(ClashNode {
            name: s(m, "name").unwrap_or_else(|| format!("{kind} node")),
            kind,
            server: s(m, "server").unwrap_or_default(),
            port: num(m, "port").unwrap_or(0) as u16,
            private_key: priv_raw,
            public_key: pub_raw,
            wg_private_key,
            wg_public_key,
            ip: s(m, "ip"),
            ipv6: s(m, "ipv6"),
            mtu: num(m, "mtu"),
            sni: s(m, "sni"),
            dns: m
                .get(serde_yaml::Value::String("dns".into()))
                .and_then(|v| v.as_sequence())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            udp: m
                .get(serde_yaml::Value::String("udp".into()))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        });
    }
    out
}

/// Route 1: the node's WARP credentials as a plain WireGuard config.
pub fn clash_to_wireguard(node: &ClashNode, keepalive: u64, mtu: u64) -> Result<String, String> {
    let (Some(priv_key), Some(pub_key)) = (&node.wg_private_key, &node.wg_public_key) else {
        return Err("this node carries Cloudflare multi-algorithm keys, not plain WireGuard keys — import it as MASQUE (Mihomo) instead".into());
    };
    let mut ips = node.addresses();
    if ips.is_empty() {
        ips.push("172.16.0.2/32".into());
    }
    Ok(format!(
        "[Interface]\nPrivateKey = {}\nAddress = {}\nMTU = {}\n\n[Peer]\nPublicKey = {}\nEndpoint = engage.cloudflareclient.com:2408\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = {}\n",
        priv_key,
        ips.join(", "),
        if mtu > 0 { mtu } else { node.mtu.unwrap_or(1280) },
        pub_key,
        if keepalive > 0 { keepalive } else { 60 },
    ))
}

/// Route 2: a self-contained Mihomo config that speaks MASQUE natively and
/// exposes a local socks port our fanout can dial through.
pub fn clash_to_mihomo(node: &ClashNode, port: u16) -> String {
    use serde_json::json;
    let mut proxy = json!({
        "name": node.name,
        "type": node.kind,
        "server": node.server,
        "port": node.port,
        "private-key": node.private_key,
        "public-key": node.public_key,
        "ip": node.ip.clone().unwrap_or_else(|| "172.16.0.2/32".into()),
        "ipv6": node.ipv6.clone().unwrap_or_default(),
        "mtu": node.mtu.unwrap_or(1280),
        "udp": node.udp,
        "sni": node.sni.clone().unwrap_or_else(|| "www.microsoft.com".into()),
    });
    // remote-dns-resolve needs a resolver list; nodes without one must not
    // get an empty `dns: []`, mihomo rejects that
    if !node.dns.is_empty() {
        if let Some(obj) = proxy.as_object_mut() {
            obj.insert("remote-dns-resolve".into(), json!(true));
            obj.insert("dns".into(), json!(node.dns.clone()));
        }
    }
    let doc = json!({
        "mixed-port": port,
        "allow-lan": false,
        "mode": "global",
        "log-level": "warning",
        "ipv6": true,
        "proxies": [proxy],
        "proxy-groups": [
            { "name": "GLOBAL", "type": "select", "proxies": [node.name.clone()] }
        ],
        "rules": ["MATCH,GLOBAL"]
    });
    serde_yaml::to_string(&doc).unwrap_or_default()
}

pub fn supervisor(state: Arc<AppState>) {
    tokio::spawn(async move {
        use std::sync::atomic::{AtomicBool, Ordering};
        let was_enabled = AtomicBool::new(false);
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            let cfg = state.config().await;
            if !cfg.warp.enabled {
                if was_enabled.swap(false, Ordering::SeqCst) {
                    if interface_exists(&cfg.warp.interface).await {
                        disconnect(&state).await;
                    }
                    let taken = {
                        MASQUE_SLOT
                            .get_or_init(|| std::sync::Mutex::new(None))
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                    };
                    if let Some(mut c) = taken {
                        let _ = c.kill().await;
                    }
                }
                clear_ports(&state).await;
                continue;
            }
            was_enabled.store(true, Ordering::SeqCst);
            run_cycle(&state).await;

            // keep the MASQUE sidecar alive when one is configured
            let (alive, cfg_port) = {
                let mut guard = MASQUE_SLOT
                    .get_or_init(|| std::sync::Mutex::new(None))
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let alive = match guard.as_mut() {
                    Some(c) => matches!(c.try_wait(), Ok(None)),
                    None => false,
                };
                (alive, cfg.warp.mihomo_port)
            };
            if tokio::fs::metadata(&cfg.warp.mihomo_conf).await.map(|m| m.len() > 0).unwrap_or(false) {
                if !alive {
                    match start_masque(&cfg, state.clone()).await {
                        Ok(()) => info!("masque sidecar started"),
                        Err(e) => warn!(error = %e, "masque sidecar start failed"),
                    }
                } else {
                    ensure_proxy_listener(&state, cfg_port).await;
                    classify_masque(&state, cfg_port).await;
                }
            }
        }
    });
}

async fn start_masque(cfg: &crate::config::Config, state: Arc<AppState>) -> Result<(), String> {
    if which(&cfg.warp.mihomo_bin).is_none() {
        return Err(format!("{} not installed", cfg.warp.mihomo_bin));
    }
    let conf = PathBuf::from(&cfg.warp.mihomo_conf);
    if let Some(parent) = conf.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    let child = Command::new(&cfg.warp.mihomo_bin)
        .args([
            "-d",
            conf.parent().unwrap_or(Path::new(".")).to_string_lossy().as_ref(),
            "-f",
            conf.to_string_lossy().as_ref(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| e.to_string())?;
    let slot = MASQUE_SLOT.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    // give it a moment to bind before we dial
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure_proxy_listener(&state, cfg.warp.mihomo_port).await;
    classify_masque(&state, cfg.warp.mihomo_port).await;
    Ok(())
}

static MASQUE_SLOT: std::sync::OnceLock<std::sync::Mutex<Option<tokio::process::Child>>> =
    std::sync::OnceLock::new();

/// Expose the MASQUE sidecar's local socks port as a fanout port.
/// The port is recorded only after the listener actually bound, so a bind
/// failure (e.g. an orphaned mihomo still holding the port after a service
/// restart) is retried on the next tick instead of being remembered forever.
async fn ensure_proxy_listener(state: &Arc<AppState>, port: u16) {
    static PROXY_LISTENERS: std::sync::OnceLock<std::sync::Mutex<Vec<u16>>> =
        std::sync::OnceLock::new();
    let set = PROXY_LISTENERS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if set.lock().unwrap_or_else(|e| e.into_inner()).contains(&port) {
        return;
    }
    let st = state.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let dialer = Dialer::Proxy(format!("socks5://127.0.0.1:{port}"));
        if let Err(e) = relay::run_listener_signaled(st, port, dialer, tx).await {
            warn!(port, error = %e, "masque fanout port bind failed (will retry)");
        }
    });
    // resolves only after the bind succeeded (or the task died)
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(())) => {
            set.lock().unwrap_or_else(|x| x.into_inner()).push(port);
            info!(port, "masque fanout port listening");
        }
        Ok(Err(_)) => warn!(port, "masque listener died before binding"),
        Err(_) => warn!(port, "masque listener bind timed out"),
    }
}

async fn classify_masque(state: &Arc<AppState>, port: u16) {
    let cfg = state.config().await;
    // throttle: only re-classify every 10 minutes
    if let Ok(text) = tokio::fs::read_to_string(state.data_dir.join("warp").join("masque.json")).await {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(ts) = v.get("last_check").and_then(|x| x.as_i64()) {
                if now_ts() - ts < 600 {
                    return;
                }
            }
        }
    }
    let key = format!("socks5://127.0.0.1:{port}");
    if let Some((exit, latency)) = crate::checker::classify_via_proxy(&key, &cfg).await {
        let dir = state.data_dir.join("warp");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let _ = tokio::fs::write(
            dir.join("masque.json"),
            json!({
                "exit_ip": exit.ip, "country": exit.country, "country_code": exit.country_code,
                "isp": exit.isp, "hosting": exit.hosting, "latency_ms": latency,
                "port": port, "last_check": now_ts()
            })
            .to_string(),
        )
        .await;
        info!(%exit.ip, country = ?exit.country_code, latency, "masque exit classified");
        add_tunnel_entry(state, port, "masque", "masque-node").await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_clash_keys() {
        let priv_clash = "MHcCAQEEIOTDZ2O+jojF/i+gswHEZW8RVLYQ8YrAcKMOOSy+Nz4DoAoGCCqGSM49AwEHoUQDQgAEkNQD0H3ZhQD+/UrHTKZMdERrJeRS9j3y4WPgLni5sbfyLJLyT9PJI8s0DDiM1j40S17Nv1Kdk3SzG9Af7UrE4w==";
        let pub_clash = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEIaU7MToJm9NKp8YfGxR6r+/h4mcG7SxI8tsW8OR1A5tv/zCzVbCRRh2t87/kxnP6lAy0lkr7qYwu+ox+k3dr6w==";
        // the private blob's OCTET STRING(32) is the X25519 private key ...
        assert_eq!(
            normalize_key(priv_clash).as_deref(),
            Some("5MNnY76OiMX+L6CzAcRlbxFUthDxisBwow45LL43PgM=")
        );
        // ... but the public blob holds multi-algorithm material, so we must
        // not silently hand a random chunk out as "the key"
        assert_eq!(normalize_key(pub_clash), None);
    }

    #[test]
    fn keeps_raw_32_byte_keys() {
        assert_eq!(
            normalize_key("5MNnY76OiMX+L6CzAcRlbxFUthDxisBwow45LL43PgM=").as_deref(),
            Some("5MNnY76OiMX+L6CzAcRlbxFUthDxisBwow45LL43PgM=")
        );
    }
}
