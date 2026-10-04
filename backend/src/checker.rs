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
    /// The classifier endpoint answered 429. That says nothing about the proxy
    /// under test, so the outcome must not be allowed to change proxy state.
    pub rate_limited: bool,
}

#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    path: String,
    /// `checker.classify_url` was configured with an `https://` URL.
    ///
    /// The check always speaks cleartext HTTP — an absolute-URI GET for a
    /// forward proxy, a bare GET through SOCKS. Handing it an https URL used to
    /// silently produce a plaintext request against port 443, which can never
    /// succeed, so every proxy looked dead and the pool went dark with the
    /// cause visible only at `debug` level. We detect it and refuse the round
    /// loudly instead.
    https: bool,
}

impl Target {
    fn from_url(url: &str) -> Target {
        let (https, rest) = if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else {
            (false, url)
        };
        let default_port: u16 = if https { 443 } else { 80 };
        let (netloc, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match netloc.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
            None => (netloc.to_string(), default_port),
        };
        Target { host, port, path: path.to_string(), https }
    }
}

/// Log the unsupported-https misconfiguration once per round.
fn warn_https_classifier(url: &str) {
    tracing::error!(
        url,
        "checker.classify_url is an https:// URL but the liveness check speaks cleartext HTTP \
         through the proxy — skipping this round instead of marking every proxy dead. \
         Point classify_url at an http:// endpoint (the shipped ip-api.com default is fine)."
    );
}

/// Distinguishes "the classifier refused to answer" from "the proxy is broken".
#[derive(Debug)]
enum ClassifyError {
    RateLimited,
    Other(anyhow::Error),
}

impl std::fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => f.write_str("classifier rate limited (HTTP 429)"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClassifyError {}

impl From<anyhow::Error> for ClassifyError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<std::io::Error> for ClassifyError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.into())
    }
}

impl From<serde_json::Error> for ClassifyError {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(e.into())
    }
}

fn dead() -> Outcome {
    Outcome { alive: false, latency_ms: None, exit: None, rate_limited: false }
}

fn rate_limited() -> Outcome {
    Outcome { alive: false, latency_ms: None, exit: None, rate_limited: true }
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
    if target.https {
        warn_https_classifier(&cfg.checker.classify_url);
        return;
    }

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
    let mut throttled = 0usize;
    for h in handles {
        let Ok((key, out)) = h.await else { continue };
        if out.rate_limited {
            throttled += 1;
        }
        if out.alive {
            ok += 1;
        }
        apply_outcome(state, &key, out).await;
    }
    if throttled > 0 {
        tracing::warn!(
            throttled,
            total,
            "classifier rate-limited this round (HTTP 429) — those proxies were left untouched. \
             ip-api.com's free tier allows ~45 requests/min per IP, so lower checker.concurrency \
             or use a classifier with a higher quota; otherwise a large round cannot succeed."
        );
    }
    state.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(total, alive = ok, "check round finished");
}

async fn apply_outcome(state: &Arc<AppState>, key: &str, out: Outcome) {
    let mut map = state.proxies.write().await;
    let Some(p) = map.get_mut(key) else { return };
    if out.rate_limited {
        // The classifier throttled us; this tells us nothing about the proxy,
        // so do not touch its state (not even last_check).
        return;
    }
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
            // Drop the fields that can only come from a *successful*
            // classification, so a dead node stops reporting the exit IP and
            // `hosting` flag of its last good check. `country`/`country_code`/
            // `isp` are deliberately kept: the source list also supplies those
            // and the next refresh re-merges them anyway.
            p.hosting = None;
            p.anon_flag = None;
            p.exit_ip = None;
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
            rate_limited: false,
        },
        Ok(Ok(None)) => Outcome {
            alive: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
            exit: None,
            rate_limited: false,
        },
        Ok(Err(ClassifyError::RateLimited)) => {
            debug!(key, "classifier throttled the request");
            rate_limited()
        }
        Ok(Err(ClassifyError::Other(e))) => {
            debug!(key, error = %e, "check failed");
            dead()
        }
        Err(_) => dead(),
    }
}

async fn run_check(key: &str, target: &Target) -> Result<Option<ExitInfo>, ClassifyError> {
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

async fn raw_check(s: &mut TcpStream, target: &Target) -> Result<Option<ExitInfo>, ClassifyError> {
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

fn finish(buf: &[u8], _target: &Target) -> Result<Option<ExitInfo>, ClassifyError> {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no http header in response"))?;
    let head = &buf[..split];
    let first = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let status = String::from_utf8_lossy(first);
    if !(status.starts_with("HTTP/") && status.contains(" 2")) {
        // 429 means the classifier throttled us, not that the proxy is broken.
        // Flag it so the caller can leave the proxy's state alone.
        if status.contains(" 429") {
            return Err(ClassifyError::RateLimited);
        }
        return Err(ClassifyError::Other(anyhow::anyhow!(
            "bad status: {}",
            status.trim()
        )));
    }
    let body = &buf[split + 4..];
    let v: Value = serde_json::from_slice(body)?;
    if jstr(&v, "status") != Some("success") {
        return Err(ClassifyError::Other(anyhow::anyhow!(
            "classifier error: {}",
            jstr(&v, "message").unwrap_or("unknown")
        )));
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

/// Classify any upstream proxy (http/socks4/socks5) — same classifier
/// request, but dialling through the proxy instead of a local tunnel IP.
pub(crate) async fn classify_via_proxy(key: &str, cfg: &crate::config::Config) -> Option<(ExitInfo, u64)> {
    let target = Target::from_url(&cfg.checker.classify_url);
    if target.https {
        warn_https_classifier(&cfg.checker.classify_url);
        return None;
    }
    let timeout = Duration::from_secs(cfg.checker.timeout_secs.max(1));
    let start = Instant::now();
    let fut = async {
        let (proto, ip, port) = split_key(key)?;
        let mut s = match proto {
            Protocol::Socks5 => relay::socks5_dial(&ip, port, &target.host, target.port).await?,
            Protocol::Socks4 => relay::socks4_dial(&ip, port, &target.host, target.port).await?,
            Protocol::Http => {
                let mut s = tokio::net::TcpStream::connect((ip.as_str(), port)).await?;
                let req = format!(
                    "GET http://{host}{path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Mozilla/5.0 resi-fanout\r\nConnection: close\r\n\r\n",
                    host = target.host, path = target.path
                );
                s.write_all(req.as_bytes()).await?;
                let buf = read_to_eof(&mut s).await?;
                return finish(&buf, &target);
            }
        };
        raw_check(&mut s, &target).await
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(Some(exit))) => Some((exit, start.elapsed().as_millis() as u64)),
        _ => None,
    }
}

/// Classify the exit of a local OpenVPN tunnel: connect with the socket
/// bound to the tunnel IP and run the same classifier request.
/// Returns (exit info, latency) or None.
pub(crate) async fn classify_from(
    local: std::net::IpAddr,
    cfg: &crate::config::Config,
) -> Option<(ExitInfo, u64)> {
    let target = Target::from_url(&cfg.checker.classify_url);
    if target.https {
        warn_https_classifier(&cfg.checker.classify_url);
        return None;
    }
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
