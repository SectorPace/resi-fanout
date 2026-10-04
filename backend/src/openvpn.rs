//! VPN Gate tunnel manager: runs one OpenVPN sidecar per selected relay
//! server, each with its own tun device, source-policy routing table and
//! local SOCKS fanout port. The host's default route is never touched
//! (route-nopull + `ip rule from <tun-ip> table <N>`).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::models::{now_ts, VpnTunnel};
use crate::relay::{self, Dialer};
use crate::state::AppState;

const MAX_ATTEMPTS: u32 = 3;
const CLASSIFY_EVERY_SECS: i64 = 15 * 60;
/// A child that is alive but never publishes its tun address holds its slot
/// forever otherwise: `resolv-retry infinite` means openvpn retries name
/// resolution indefinitely and never exits, so liveness is no evidence of
/// progress.
const TUNNEL_START_DEADLINE_SECS: i64 = 120;

struct RunningVpn {
    child: Child,
    relay: Option<JoinHandle<()>>,
    last_tun: Option<IpAddr>,
    started_at: i64,
}

pub fn spawn_supervisor(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut running: HashMap<u16, RunningVpn> = HashMap::new();
        let mut warned_no_bin = false;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let cfg = state.config().await;

            if !cfg.vpngate.enabled {
                if !running.is_empty() {
                    stop_all(&mut running, &state).await;
                }
                continue;
            }
            if which(&cfg.vpngate.openvpn_bin).is_none() {
                if !warned_no_bin {
                    warned_no_bin = true;
                    warn!(
                        bin = %cfg.vpngate.openvpn_bin,
                        "vpngate enabled but openvpn not found — install it (e.g. apt install openvpn)"
                    );
                }
                continue;
            }
            warned_no_bin = false;
            reconcile(&state, &cfg, &mut running).await;
        }
    });
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.exists().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.exists())
}

#[allow(clippy::too_many_lines)]
async fn reconcile(state: &Arc<AppState>, cfg: &crate::config::Config, running: &mut HashMap<u16, RunningVpn>) {
    let vg = &cfg.vpngate;

    // 0) tunnels disabled by max_servers=0
    if vg.max_servers == 0 {
        if !running.is_empty() {
            stop_all(running, state).await;
        }
        return;
    }

    let mut changed = false;
    {
        let pool = state.vpn_pool.read().await;
        let mut tunnels = state.vpn_tunnels.write().await;
        let pool_has = |key: &str| pool.iter().any(|s| s.server_key() == key);
        let pool_has = &pool_has;

        // 1) drop tunnels whose server vanished from the pool
        tunnels.retain(|t| {
            if pool_has(&t.server_key) {
                true
            } else {
                info!(key = %t.server_key, "vpngate: server left the pool, dropping tunnel");
                changed = true;
                false
            }
        });

        // 2) rotate exhausted tunnels (repeated failures / non-residential)
        tunnels.retain(|t| {
            if t.status == "failed" && t.attempts >= MAX_ATTEMPTS {
                info!(key = %t.server_key, "vpngate: rotating out failed tunnel");
                changed = true;
                return false;
            }
            if t.status == "blacklisted" {
                changed = true;
                return false;
            }
            true
        });

        // 3) top up to max_servers from the ranked pool
        let ranked = crate::vpngate::rank(&pool, &vg.countries, vg.min_speed_mbps);
        let mut used_ports: Vec<u16> = tunnels.iter().map(|t| t.local_port).collect();
        for s in ranked {
            if tunnels.len() >= vg.max_servers as usize {
                break;
            }
            let key = s.server_key();
            if tunnels.iter().any(|t| t.server_key == key) {
                continue;
            }
            let Some(port) = next_free_port(&used_ports, vg.base_port, vg.max_servers as usize) else {
                break;
            };
            used_ports.push(port);
            info!(server = %s.hostname, country = ?s.country_short, port, "vpngate: selected server");
            tunnels.push(VpnTunnel {
                server_key: key,
                hostname: s.hostname.clone(),
                local_port: port,
                status: "spawning".into(),
                ..Default::default()
            });
            changed = true;
        }
    }

    // 4) per-tunnel supervision
    let mut dirty = changed;
    let vpn_dir = state.data_dir.join("vpn");
    let _ = tokio::fs::create_dir_all(&vpn_dir).await;
    let tunnels = state.vpn_tunnels.read().await.clone();
    for t in &tunnels {
        let entry = running.get_mut(&t.local_port);

        // 4a) spawn missing children
        if entry.is_none() {
            match spawn_openvpn(state, cfg, t, &vpn_dir).await {
                Ok(child) => {
                    running.insert(
                        t.local_port,
                        RunningVpn { child, relay: None, last_tun: None, started_at: now_ts() },
                    );
                    mark(state, &t.server_key, |x| x.status = "spawning".into()).await;
                    dirty = true;
                }
                Err(e) => {
                    warn!(port = t.local_port, error = %e, "vpngate: spawn failed");
                    mark(state, &t.server_key, |x| {
                        x.attempts += 1;
                        x.status = if x.attempts >= MAX_ATTEMPTS {
                            "failed".into()
                        } else {
                            "down".into()
                        };
                    })
                    .await;
                    dirty = true;
                }
            }
            continue;
        }

        let Some(r) = running.get_mut(&t.local_port) else { continue };

        // 4b) exited?
        match r.child.try_wait() {
            Ok(Some(_status)) => {
                info!(port = t.local_port, "vpngate: openvpn process exited");
                if let Some(h) = r.relay.take() {
                    h.abort();
                }
                running.remove(&t.local_port);
                mark(state, &t.server_key, |x| {
                    x.attempts += 1;
                    x.status = if x.attempts >= MAX_ATTEMPTS { "failed".into() } else { "down".into() };
                })
                .await;
                dirty = true;
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                warn!(port = t.local_port, error = %e, "vpngate: try_wait failed");
                continue;
            }
        }

        // 4c) tunnel came up? (up-script wrote the tun ip)
        let ipfile = vpn_dir.join(format!("tunnel-{}.ip", t.local_port));
        if let Ok(text) = tokio::fs::read_to_string(&ipfile).await {
            if let Ok(ip) = text.trim().parse::<IpAddr>() {
                if r.last_tun != Some(ip) {
                    info!(port = t.local_port, %ip, "vpngate: tunnel is up");
                    if let Some(h) = r.relay.take() {
                        h.abort();
                    }
                    let st = state.clone();
                    let port = t.local_port;
                    r.relay = Some(tokio::spawn(async move {
                        let _ = relay::run_listener(st, port, Dialer::Tun(ip)).await;
                    }));
                    r.last_tun = Some(ip);
                    mark(state, &t.server_key, |x| {
                        x.status = "up".into();
                        x.tun_ip = Some(ip.to_string());
                        x.attempts = 0; // a successful dial proves the server works
                        x.alive = false; // force re-classify
                        x.last_check = None;
                    })
                    .await;
                    dirty = true;
                }
            }
        }

        // never came up: rotate instead of holding the slot forever
        if r.last_tun.is_none() && now_ts() - r.started_at > TUNNEL_START_DEADLINE_SECS {
            warn!(
                port = t.local_port,
                server = %t.server_key,
                "vpngate: tunnel did not come up within {}s, rotating",
                TUNNEL_START_DEADLINE_SECS
            );
            if let Some(h) = r.relay.take() {
                h.abort();
            }
            stop_openvpn(&mut r.child).await;
            running.remove(&t.local_port);
            let _ = tokio::fs::remove_file(&ipfile).await;
            mark(state, &t.server_key, |x| {
                x.attempts += 1;
                x.status = if x.attempts >= MAX_ATTEMPTS { "failed".into() } else { "down".into() };
            })
            .await;
            dirty = true;
            continue;
        }

        // 4d) classify through the tunnel (residential detection)
        let need_classify = match (t.status.as_str(), t.last_check) {
            ("up", None) => true,
            ("up", Some(ts)) => now_ts() - ts > CLASSIFY_EVERY_SECS,
            _ => false,
        };
        if need_classify {
            if let Some(ip) = r.last_tun {
                match crate::checker::classify_from(ip, cfg).await {
                    Some((exit, latency)) => {
                        let resi = exit.hosting == Some(false);
                        info!(
                            port = t.local_port,
                            exit = %exit.ip,
                            country = ?exit.country_code,
                            residential = resi,
                            "vpngate: classified tunnel exit"
                        );
                        let only_resi = cfg.vpngate.only_residential;
                        mark(state, &t.server_key, |x| {
                            x.alive = true;
                            x.latency_ms = Some(latency);
                            x.country = exit.country.clone();
                            x.country_code = exit.country_code.clone();
                            x.isp = exit.isp.clone();
                            x.hosting = exit.hosting;
                            x.exit_ip = Some(exit.ip);
                            x.last_check = Some(now_ts());
                            if only_resi && !resi {
                                x.status = "blacklisted".into(); // datacenter, tear down
                            }
                        })
                        .await;
                        if only_resi && !resi {
                            if let Some(rv) = running.get_mut(&t.local_port) {
                                stop_openvpn(&mut rv.child).await;
                            }
                        }
                        dirty = true;
                    }
                    None => {
                        mark(state, &t.server_key, |x| x.last_check = Some(now_ts())).await;
                    }
                }
            }
        }
    }

    // 5) kill tunnels that disappeared from the list
    let wanted: Vec<u16> = tunnels.iter().map(|t| t.local_port).collect();
    for port in running.keys().cloned().collect::<Vec<_>>() {
        if !wanted.contains(&port) {
            if let Some(mut rv) = running.remove(&port) {
                if let Some(h) = rv.relay.take() {
                    h.abort();
                }
                stop_openvpn(&mut rv.child).await;
                let _ = tokio::fs::remove_file(state.data_dir.join("vpn").join(format!("tunnel-{port}.ip"))).await;
                info!(port, "vpngate: tunnel stopped");
            }
        }
    }

    if dirty {
        state.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Directives a remote VPN Gate config is never allowed to set.
///
/// The config arrives base64-encoded inside a third-party HTTP response and is
/// then handed to openvpn with `script-security 2` and full `CAP_NET_ADMIN`,
/// so anything reachable from here is remote code execution or host-routing
/// takeover. Grouped by what they would let the server do.
const FORBIDDEN_DIRECTIVES: &[&str] = &[
    // run arbitrary programs
    "up", "down", "route-up", "ipchange", "client-connect", "learn-address",
    "tls-verify", "plugin", "config", "cd", "chroot", "daemon", "askpass",
    // environment / privileges / config we own
    "setenv", "setenv-safe", "script-security", "user", "group", "auth-user-pass",
    // take over host routing (we use route-nopull + `ip rule from ... table N`)
    "route", "route-ipv6", "route-delay", "iproute", "redirect-gateway",
    "redirect-private", "pull", "pull-filter", "client-config-dir",
    // devices / logging we set ourselves
    "dev", "dev-node", "dev-type", "log", "log-append", "status", "writepid",
    "management", "management-client", "management-query-passwords",
];

/// The directive name of one OpenVPN config line, lowercased, or `None` for
/// blanks/comments.
///
/// Handles the two shapes that made a naive `starts_with("up ")` unsafe:
/// arbitrary runs of spaces/tabs between the directive and its first argument,
/// and the optional `--` prefix that OpenVPN also accepts in config files.
fn directive_of(line: &str) -> Option<String> {
    let t = line.trim_start_matches([' ', '\t']);
    if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
        return None;
    }
    let t = t.strip_prefix("--").unwrap_or(t);
    let tok = t.split([' ', '\t']).next()?;
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_ascii_lowercase())
    }
}

/// Drop forbidden directives from a remote .ovpn, preserving inline blocks.
///
/// `<ca>`, `<cert>`, `<key>` and `<tls-auth>` payloads span many lines and are
/// *not* directives, so their contents are copied through verbatim; filtering
/// them would corrupt the tunnel material.
fn sanitize_remote_config(base: &str, port: u16) -> String {
    let mut out = String::with_capacity(base.len());
    let mut in_block = false;
    for line in base.lines() {
        let t = line.trim();
        if in_block {
            out.push_str(line);
            out.push('\n');
            if t.contains("</") {
                in_block = false;
            }
            continue;
        }
        if t.starts_with('<') {
            // opening tag (possibly with the payload on the same line)
            in_block = !t.contains("</");
            out.push_str(line);
            out.push('\n');
            continue;
        }
        match directive_of(line) {
            Some(d) if FORBIDDEN_DIRECTIVES.contains(&d.as_str()) => {
                warn!(directive = %d, port, "vpngate: dropped directive from remote config");
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

async fn spawn_openvpn(
    state: &Arc<AppState>,
    cfg: &crate::config::Config,
    t: &VpnTunnel,
    vpn_dir: &PathBuf,
) -> anyhow::Result<Child> {
    use base64::engine::general_purpose::STANDARD as B64;

    let server = state
        .vpn_pool
        .read()
        .await
        .iter()
        .find(|s| s.server_key() == t.server_key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("server {} not in pool", t.server_key))?;

    let base_conf = String::from_utf8(B64.decode(&server.config_b64)?)
        .map_err(|_| anyhow::anyhow!("config is not valid utf8"))?;

    // Strip directives a third-party VPN Gate config must never control.
    // This has to match the *parsed* directive name, not a literal prefix:
    // OpenVPN treats spaces AND tabs as parameter delimiters (that is why the
    // manual documents `[SPACE]` for escaping a literal one), so the previous
    // `starts_with("up ")` check was trivially bypassed by `up\t/tmp/evil.sh`.
    let mut conf = sanitize_remote_config(&base_conf, t.local_port);

    let ipfile = vpn_dir.join(format!("tunnel-{}.ip", t.local_port));
    let logfile = vpn_dir.join(format!("tunnel-{}.log", t.local_port));
    let authfile = vpn_dir.join("auth.txt");
    let up = PathBuf::from(&cfg.vpngate.scripts_dir).join("vpn-up.sh");
    let down = PathBuf::from(&cfg.vpngate.scripts_dir).join("vpn-down.sh");
    tokio::fs::write(&authfile, "vpn\nvpn\n").await?;
    let _ = tokio::fs::remove_file(&ipfile).await;

    conf.push_str(&format!(
        "\n# --- resi-fanout appended ---\n\
         route-nopull\n\
         dev tunrf{}\n\
         script-security 2\n\
         auth-user-pass {}\n\
         setenv VPN_TABLE {}\n\
         setenv VPN_IPFILE {}\n\
         up {}\n\
         down {}\n\
         log-append {}\n\
         data-ciphers-fallback AES-256-CBC\n\
         resolv-retry infinite\n\
         connect-retry-max 2\n\
         connect-timeout 10\n\
         ping 10\n\
         ping-restart 40\n\
         verb 3\n",
        t.local_port,
        authfile.display(),
        t.local_port,
        ipfile.display(),
        up.display(),
        down.display(),
        logfile.display(),
    ));

    let confpath = vpn_dir.join(format!("tunnel-{}.ovpn", t.local_port));
    tokio::fs::write(&confpath, conf).await?;

    info!(port = t.local_port, server = %server.hostname, "vpngate: starting openvpn");
    let child = tokio::process::Command::new(&cfg.vpngate.openvpn_bin)
        .arg("--config")
        .arg(&confpath)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(child)
}

/// Tear an openvpn child down *gracefully*.
///
/// `Child::kill()` sends SIGKILL, which openvpn cannot handle: it never runs its
/// `down` hook, so `scripts/vpn-down.sh` never removes the
/// `ip rule from <tun-ip> lookup <table>` and the table route that
/// `scripts/vpn-up.sh` installed. Every rotation (blacklisted exit, server
/// leaving the pool, feature disabled) therefore leaked one kernel rule keyed
/// to the dead tunnel IP. SIGTERM lets openvpn tear down cleanly; SIGKILL is
/// only the fallback if it ignores the signal.
async fn stop_openvpn(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: `pid` is a child this supervisor spawned and still owns, and
        // kill(2) takes no pointers into our address space.
        let term_ok = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 };
        if term_ok {
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_ok() {
                return;
            }
            warn!("openvpn ignored SIGTERM, escalating to SIGKILL");
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn mark(state: &Arc<AppState>, server_key: &str, f: impl FnOnce(&mut VpnTunnel)) {
    let mut tunnels = state.vpn_tunnels.write().await;
    if let Some(t) = tunnels.iter_mut().find(|t| t.server_key == server_key) {
        f(t);
    }
}

async fn stop_all(running: &mut HashMap<u16, RunningVpn>, state: &Arc<AppState>) {
    for (port, mut rv) in running.drain() {
        if let Some(h) = rv.relay.take() {
            h.abort();
        }
        stop_openvpn(&mut rv.child).await;
        let _ = tokio::fs::remove_file(state.data_dir.join("vpn").join(format!("tunnel-{port}.ip"))).await;
        info!(port, "vpngate: tunnel stopped (disabled)");
    }
}

fn next_free_port(used: &[u16], base: u16, max: usize) -> Option<u16> {
    for i in 0..max {
        let cand = base as u32 + i as u32;
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
