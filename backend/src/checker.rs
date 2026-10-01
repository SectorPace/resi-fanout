//! Health checking + residential classification.
//!
//! Every candidate is asked to fetch a plain-HTTP classifier endpoint
//! (default: ip-api.com) *through itself*. That proves liveness, measures
//! latency, reveals the exit IP, and returns the `hosting` flag —
//! `hosting == false` means the exit IP belongs to a consumer/ISP line,
//! i.e. residential-ish, rather than a datacenter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tracing::debug;

use crate::models::{now_ts, split_key, Protocol};
use crate::relay;
use crate::state::AppState;

#[derive(Clone, Debug)]
pub struct ExitInfo {
    pub ip: String,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub isp: Option<String>,
    pub hosting: Option<bool>,
    pub proxy_flag: Option<bool>,
}

pub struct Outcome {
    pub alive: bool,
    pub latency_ms: Option<u64>,
    pub exit: Option<ExitInfo>,
}

#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn from_url(url: &str) -> Target {
        let rest = url
            .strip_prefix("http://")
            .unwrap_or_else(|| url.strip_prefix("https://").unwrap_or(url));
        let (netloc, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match netloc.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80u16)),
            None => (netloc.to_string(), 80),
        };
        Target { host, port, path: path.to_string() }
    }
}

fn dead() -> Outcome {
    Outcome { alive: false, latency_ms: None, exit: None }
}

/// Check the given keys (or the whole pool) with bounded concurrency,
/// then apply results to the state.
pub async fn check_all(state: &Arc<AppState>, keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    let cfg = state.config().await;
    let timeout = Duration::from_secs(cfg.checker.timeout_secs.max(1));
    let sem = Arc::new(Semaphore::new(cfg.checker.concurrency.max(1)));
    let target = Target::from_url(&cfg.checker.classify_url);

    let mut handles = Vec::with_capacity(keys.len());
    for key in keys {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        let target = target.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let out = run_checked(&key, timeout, &target).await;
            (key, out)
        }));
    }

    let total = handles.len();
    let mut ok = 0usize;
    for h in handles {
        let Ok((key, out)) = h.await else { continue };
        if out.alive {
            ok += 1;
        }
        apply_outcome(state, &key, out).await;
    }
    state.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(total, alive = ok, "check round finished");
}

async fn apply_outcome(state: &Arc<AppState>, key: &str, out: Outcome) {
    let mut map = state.proxies.write().await;
    let Some(p) = map.get_mut(key) else { return };
    p.last_check = Some(now_ts());
    if out.alive {
        p.alive = true;
        p.fails = 0;
        p.latency_ms = out.latency_ms;
        if let Some(e) = out.exit {
            p.exit_ip = Some(e.ip);
            if e.country.is_some() {
                p.country = e.country;
            }
            if e.country_code.is_some() {
                p.country_code = e.country_code;
            }
            if e.isp.is_some() {
                p.isp = e.isp;
            }
            p.hosting = e.hosting;
            p.anon_flag = e.proxy_flag;
        }
    } else {
        p.fails += 1;
        if p.fails >= 2 {
            p.alive = false;
            p.local_port = None;
            p.latency_ms = None;
        }
    }
}

async fn run_checked(key: &str, timeout: Duration, target: &Target) -> Outcome {
    let start = Instant::now();
    match tokio::time::timeout(timeout, run_check(key, target)).await {
        Ok(Ok(Some(exit))) => Outcome {
            alive: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
            exit: Some(exit),
        },
        Ok(Ok(None)) => Outcome {
            alive: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
            exit: None,
        },
        Ok(Err(e)) => {
            debug!(key, error = %e, "check failed");
            dead()
        }
        Err(_) => dead(),
    }
}

async fn run_check(key: &str, target: &Target) -> anyhow::Result<Option<ExitInfo>> {
    let (proto, ip, port) = split_key(key)?;
    match proto {
        Protocol::Http => {
            // classic forward-proxy check: absolute-URI GET over plain HTTP
            let mut s = TcpStream::connect((ip.as_str(), port)).await?;
            s.set_nodelay(true).ok();
            let req = format!(
                "GET http://{host}{path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Mozilla/5.0 resi-fanout\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                host = target.host,
                path = target.path
            );
            s.write_all(req.as_bytes()).await?;
            let buf = read_to_eof(&mut s).await?;
            finish(&buf, target)
        }
        Protocol::Socks5 => {
            let mut s = relay::socks5_dial(&ip, port, &target.host, target.port).await?;
            raw_check(&mut s, target).await
        }
        Protocol::Socks4 => {
            let mut s = relay::socks4_dial(&ip, port, &target.host, target.port).await?;
            raw_check(&mut s, target).await
        }
    }
}

async fn raw_check(s: &mut TcpStream, target: &Target) -> anyhow::Result<Option<ExitInfo>> {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Mozilla/5.0 resi-fanout\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path = target.path,
        host = target.host
    );
    s.write_all(req.as_bytes()).await?;
    let buf = read_to_eof(s).await?;
    finish(&buf, target)
}

async fn read_to_eof(s: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 256 * 1024 {
            break;
        }
    }
    Ok(buf)
}

fn finish(buf: &[u8], _target: &Target) -> anyhow::Result<Option<ExitInfo>> {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no http header in response"))?;
    let head = &buf[..split];
    let first = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let status = String::from_utf8_lossy(first);
    if !(status.starts_with("HTTP/") && status.contains(" 2")) {
        anyhow::bail!("bad status: {}", status.trim());
    }
    let body = &buf[split + 4..];
    let v: Value = serde_json::from_slice(body)?;
    if jstr(&v, "status") != Some("success") {
        anyhow::bail!(
            "classifier error: {}",
            jstr(&v, "message").unwrap_or("unknown")
        );
    }
    Ok(Some(ExitInfo {
        ip: jstr(&v, "query").unwrap_or("").to_string(),
        country: jstr(&v, "country").map(String::from),
        country_code: jstr(&v, "countryCode").map(String::from),
        isp: jstr(&v, "isp").map(String::from),
        hosting: v.get("hosting").and_then(|x| x.as_bool()),
        proxy_flag: v.get("proxy").and_then(|x| x.as_bool()),
    }))
}

fn jstr<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Convenience wrapper used by the "check" command and API.
pub async fn check_everything(state: &Arc<AppState>) {
    let keys: Vec<String> = state.proxies.read().await.keys().cloned().collect();
    check_all(state, keys).await;
    state.assign_ports().await;
    let _ = state.save_state().await;
}

/// Classify the exit of a local OpenVPN tunnel: connect with the socket
/// bound to the tunnel IP and run the same classifier request.
/// Returns (exit info, latency) or None.
pub(crate) async fn classify_from(
    local: std::net::IpAddr,
    cfg: &crate::config::Config,
) -> Option<(ExitInfo, u64)> {
    let target = Target::from_url(&cfg.checker.classify_url);
    let timeout = Duration::from_secs(cfg.checker.timeout_secs.max(1));
    let start = Instant::now();
    let fut = async {
        let mut s = crate::relay::dial_from_ip(local, &target.host, target.port).await?;
        raw_check(&mut s, &target).await
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(Some(exit))) => Some((exit, start.elapsed().as_millis() as u64)),
        _ => None,
    }
}
