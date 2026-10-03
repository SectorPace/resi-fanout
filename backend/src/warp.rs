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
    add_port_entry(state, cfg.warp.local_port).await;
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

async fn add_port_entry(state: &Arc<AppState>, port: u16) {
    // reuse the tunnel list so /api/ports and 3x-ui linking pick it up
    let mut tunnels = state.vpn_tunnels.write().await;
    if let Some(t) = tunnels.iter_mut().find(|t| t.local_port == port) {
        t.status = "up".into();
    } else {
        tunnels.push(crate::models::VpnTunnel {
            server_key: "warp".into(),
            hostname: "cloudflare-warp".into(),
            local_port: port,
            status: "up".into(),
            attempts: 0,
            alive: true,
            last_check: Some(now_ts()),
            ..Default::default()
        });
    }
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

/// strip local port entry when disabled
pub async fn clear_port(state: &Arc<AppState>) {
    let cfg = state.config().await;
    let mut tunnels = state.vpn_tunnels.write().await;
    tunnels.retain(|t| t.local_port != cfg.warp.local_port || t.server_key != "warp");
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
                if was_enabled.swap(false, Ordering::SeqCst) && interface_exists(&cfg.warp.interface).await {
                    disconnect(&state).await;
                }
                clear_port(&state).await;
                continue;
            }
            was_enabled.store(true, Ordering::SeqCst);
            run_cycle(&state).await;
        }
    });
}