//! Fanout relay: for every healthy upstream proxy we bind a local port.
//! Traffic hitting that port (SOCKS5 and/or HTTP, per config) is relayed
//! through the upstream proxy — the same idea as `gost -L :PORT -F upstream`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::FanoutMode;
use crate::models::split_key;
use crate::state::AppState;

const UPSTREAM_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const HEAD_MAX: usize = 16 * 1024;

/// How to reach the internet for one fanout port:
/// through an upstream proxy (free/paid lists) or out of a local
/// OpenVPN tunnel interface (VPN Gate), bound to its source IP.
#[derive(Clone, Debug)]
pub enum Dialer {
    Proxy(String), // proxy key "<proto>://<ip>:<port>"
    Tun(std::net::IpAddr),
}

async fn dial(dialer: &Dialer, host: &str, port: u16) -> Result<TcpStream> {
    match dialer {
        Dialer::Proxy(key) => dial_upstream(key, host, port).await,
        Dialer::Tun(ip) => dial_from_ip(*ip, host, port).await,
    }
}

/// Connect to host:port with the socket bound to `local` (the tunnel IP).
/// Reaching the remote actually happens through the tun device thanks to
/// the source policy route installed by vpn-up.sh.
pub(crate) async fn dial_from_ip(
    local: std::net::IpAddr,
    host: &str,
    port: u16,
) -> Result<TcpStream> {
    let fut = tun_connect(local, host, port);
    tokio::time::timeout(UPSTREAM_DIAL_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("tunnel dial timeout via {local}"))?
}

async fn tun_connect(local: std::net::IpAddr, host: &str, port: u16) -> Result<TcpStream> {
    let target = resolve_same_family(local, host, port).await?;
    let sock = if local.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    sock.bind(std::net::SocketAddr::new(local, 0))?;
    Ok(sock.connect(target).await?)
}

async fn resolve_same_family(
    local: std::net::IpAddr,
    host: &str,
    port: u16,
) -> Result<std::net::SocketAddr> {
    use std::net::IpAddr;
    let want_v4 = local.is_ipv4();
    let mut fallback = None;
    for a in tokio::net::lookup_host((host, port)).await? {
        match a.ip() {
            IpAddr::V4(_) if want_v4 => return Ok(a),
            IpAddr::V6(_) if !want_v4 => return Ok(a),
            _ => fallback = Some(a),
        }
    }
    fallback.ok_or_else(|| anyhow!("no {family} address for {host}", family = if want_v4 { "IPv4" } else { "IPv6" }))
}

/// Periodically recompute which local ports should be listening and
/// start/stop listeners accordingly. Poll-based on purpose: simple and
/// self-healing (a crashed listener gets restarted on the next tick).
pub fn spawn_supervisor(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut running: HashMap<u16, JoinHandle<()>> = HashMap::new();
        let mut last_bind = String::new();
        let mut last_mode: Option<FanoutMode> = None;
        let mut warned: HashSet<u16> = HashSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let cfg = state.config().await;

            if last_mode.is_some() && (cfg.fanout.bind != last_bind || Some(cfg.fanout.mode) != last_mode) {
                for (_, h) in running.drain() {
                    h.abort();
                }
                warned.clear();
            }
            last_bind = cfg.fanout.bind.clone();
            last_mode = Some(cfg.fanout.mode);

            let desired = desired_listeners(&state, &cfg).await;
            let want: HashSet<u16> = desired.iter().map(|(p, _)| *p).collect();

            // stop listeners that are no longer wanted or already dead
            let mut remove: Vec<u16> = running
                .iter()
                .filter(|(port, h)| !want.contains(*port) || h.is_finished())
                .map(|(port, _)| *port)
                .collect();
            remove.sort_unstable();
            for port in remove {
                if let Some(h) = running.remove(&port) {
                    h.abort();
                    warned.remove(&port);
                    debug!(port, "listener stopped");
                }
            }

            // start missing listeners
            for (port, key) in desired {
                if running.contains_key(&port) {
                    continue;
                }
                let st = state.clone();
                warned.remove(&port);
                running.insert(
                    port,
                    tokio::spawn(async move {
                        if let Err(e) = run_listener(st, port, Dialer::Proxy(key)).await {
                            warn!(port, error = %e, "listener exited");
                        }
                    }),
                );
            }

            // one-shot warn for ports we failed to bind
            for (port, h) in running.iter() {
                if h.is_finished() && !warned.contains(port) {
                    warned.insert(*port);
                    warn!(port, "port could not be bound (busy?) — will retry");
                }
            }
        }
    });
}

async fn desired_listeners(state: &AppState, cfg: &crate::config::Config) -> Vec<(u16, String)> {
    let map = state.proxies.read().await;
    let mut v: Vec<(u16, String)> = map
        .values()
        .filter(|p| p.alive && p.local_port.is_some() && AppState::passes_filter(cfg, p))
        .map(|p| (p.local_port.unwrap(), p.key.clone()))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Bind one fanout port and relay every session through `dialer`.
/// Used by both the proxy supervisor and the VPN Gate tunnel manager.
pub async fn run_listener(state: Arc<AppState>, port: u16, dialer: Dialer) -> Result<()> {
    let bind = state.config().await.fanout.bind;
    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow!("bind {addr}: {e}"))?;
    info!(%addr, dialer = ?dialer, "fanout port listening");
    loop {
        let (sock, peer) = listener.accept().await?;
        let st = state.clone();
        let dialer = dialer.clone();
        tokio::spawn(async move {
            let mode = st.config().await.fanout.mode;
            if let Err(e) = handle_local(mode, sock, &dialer).await {
                debug!(%peer, error = %e, "session ended");
            }
        });
    }
}

async fn handle_local(mode: FanoutMode, mut sock: TcpStream, dialer: &Dialer) -> Result<()> {
    match mode {
        FanoutMode::Socks => serve_socks5(&mut sock, dialer).await,
        FanoutMode::Http => serve_http(&mut sock, dialer).await,
        FanoutMode::Mixed => {
            let mut b = [0u8; 1];
            let n = sock.peek(&mut b).await?;
            if n > 0 && b[0] == 0x05 {
                serve_socks5(&mut sock, dialer).await
            } else {
                serve_http(&mut sock, dialer).await
            }
        }
    }
}

// ---------------------------------------------------------------- local SOCKS5

async fn serve_socks5(sock: &mut TcpStream, dialer: &Dialer) -> Result<()> {
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 {
        bail!("not a socks5 client");
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    sock.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        sock.write_all(&[0x05, 0xFF]).await?;
        bail!("client offers no no-auth method");
    }
    sock.write_all(&[0x05, 0x00]).await?;

    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        bail!("bad socks version in request");
    }
    if req[1] != 0x01 {
        sock.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
        bail!("only CONNECT is supported");
    }
    let (host, port) = read_socks_target(sock, req[3]).await?;

    let mut up = dial(dialer, &host, port).await?;
    sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(sock, &mut up).await?;
    Ok(())
}

async fn read_socks_target(sock: &mut TcpStream, atyp: u8) -> Result<(String, u16)> {
    let host = match atyp {
        0x01 => {
            let mut b = [0u8; 4];
            sock.read_exact(&mut b).await?;
            std::net::Ipv4Addr::from(b).to_string()
        }
        0x04 => {
            let mut b = [0u8; 16];
            sock.read_exact(&mut b).await?;
            std::net::Ipv6Addr::from(b).to_string()
        }
        0x03 => {
            let mut l = [0u8; 1];
            sock.read_exact(&mut l).await?;
            let mut b = vec![0u8; l[0] as usize];
            sock.read_exact(&mut b).await?;
            String::from_utf8(b)?
        }
        _ => bail!("unsupported address type {atyp}"),
    };
    let mut pb = [0u8; 2];
    sock.read_exact(&mut pb).await?;
    Ok((host, u16::from_be_bytes(pb)))
}

// ---------------------------------------------------------------- local HTTP

async fn serve_http(sock: &mut TcpStream, dialer: &Dialer) -> Result<()> {
    let head = read_head(sock, HEAD_MAX).await?;
    let text = String::from_utf8_lossy(&head);
    let reqline = text.lines().next().unwrap_or("");
    let mut parts = reqline.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() {
        bail!("empty request");
    }

    if method == "CONNECT" {
        let (host, port) = parse_authority(&target).ok_or_else(|| anyhow!("bad CONNECT target"))?;
        let mut up = dial(dialer, &host, port).await?;
        sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await?;
        tokio::io::copy_bidirectional(sock, &mut up).await?;
    } else {
        // Plain request: open a CONNECT tunnel to the origin, then forward
        // the request bytes untouched.
        let (host, port) = host_from_head(&head, &target)?;
        let mut up = dial(dialer, &host, port).await?;
        up.write_all(&head).await?;
        tokio::io::copy_bidirectional(sock, &mut up).await?;
    }
    Ok(())
}

async fn read_head(sock: &mut TcpStream, cap: usize) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before end of headers");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > cap {
            bail!("header too large");
        }
    }
}

fn parse_authority(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    let (host, port) = s.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

fn host_from_head(head: &[u8], target: &str) -> Result<(String, u16)> {
    // absolute-form request line wins
    if let Some(rest) = target.strip_prefix("http://") {
        let end = rest.find('/').unwrap_or(rest.len());
        if let Some(hp) = parse_authority(&rest[..end]) {
            return Ok(hp);
        }
        return Ok((rest[..end].to_string(), 80));
    }
    let text = String::from_utf8_lossy(head);
    for line in text.lines().skip(1) {
        if let Some(v) = header_value(line, "host:") {
            let v = v.trim();
            return Ok(parse_authority(v).unwrap_or((v.to_string(), 80)));
        }
    }
    bail!("no Host header found")
}

fn header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let idx = line.find(':')?;
    if line[..idx].eq_ignore_ascii_case(name.trim_end_matches(':')) {
        Some(&line[idx + 1..])
    } else {
        None
    }
}

// ---------------------------------------------------------------- upstream dialing

async fn dial_upstream(key: &str, host: &str, port: u16) -> Result<TcpStream> {
    use std::future::Future;
    use std::pin::Pin;
    let (proto, ip, pport) = split_key(key)?;
    let fut: Pin<Box<dyn Future<Output = Result<TcpStream>> + Send>> = match proto {
        crate::models::Protocol::Http => Box::pin(http_dial(&ip, pport, host, port)),
        crate::models::Protocol::Socks4 => Box::pin(socks4_dial(&ip, pport, host, port)),
        crate::models::Protocol::Socks5 => Box::pin(socks5_dial(&ip, pport, host, port)),
    };
    tokio::time::timeout(UPSTREAM_DIAL_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("upstream dial timeout {ip}:{pport}"))?
}

/// SOCKS5 client handshake: CONNECT via no-auth.
pub async fn socks5_dial(proxy_host: &str, proxy_port: u16, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect((proxy_host, proxy_port)).await?;
    s.set_nodelay(true).ok();
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet).await?;
    if greet[0] != 0x05 || greet[1] != 0x00 {
        bail!("socks5: greeting failed");
    }
    let mut req: Vec<u8> = vec![0x05, 0x01, 0x00];
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        req.push(0x01);
        req.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        req.push(0x04);
        req.extend_from_slice(&v6.octets());
    } else {
        req.push(0x03);
        req.push(host.len() as u8);
        req.extend_from_slice(host.as_bytes());
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        bail!("socks5: connect refused (code {})", head[1]);
    }
    let skip = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            l[0] as usize
        }
        other => bail!("socks5: bad atyp {other}"),
    };
    let mut junk = vec![0u8; skip + 2];
    s.read_exact(&mut junk).await?;
    Ok(s)
}

/// SOCKS4 client handshake (target resolved locally to IPv4 first).
pub async fn socks4_dial(proxy_host: &str, proxy_port: u16, host: &str, port: u16) -> Result<TcpStream> {
    let ip = resolve_v4(host).await?;
    let mut s = TcpStream::connect((proxy_host, proxy_port)).await?;
    s.set_nodelay(true).ok();
    let mut req = vec![0x04, 0x01];
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&ip.octets());
    req.push(0x00);
    s.write_all(&req).await?;
    let mut resp = [0u8; 8];
    s.read_exact(&mut resp).await?;
    if resp[1] != 0x5A {
        bail!("socks4: connect refused (code {})", resp[1]);
    }
    Ok(s)
}

/// HTTP proxy: open a CONNECT tunnel to host:port.
pub async fn http_dial(proxy_host: &str, proxy_port: u16, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect((proxy_host, proxy_port)).await?;
    s.set_nodelay(true).ok();
    let req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: Mozilla/5.0 resi-fanout\r\n\r\n");
    s.write_all(req.as_bytes()).await?;
    let head = read_head(&mut s, 8192).await?;
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or("");
    if !(status.starts_with("HTTP/") && status.contains(" 2")) {
        bail!("http proxy: CONNECT failed: {status}");
    }
    Ok(s)
}

pub(crate) async fn resolve_v4(host: &str) -> Result<std::net::Ipv4Addr> {
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return Ok(v4);
    }
    let addrs = tokio::net::lookup_host((host, 80u16)).await?;
    for a in addrs {
        if let std::net::IpAddr::V4(v4) = a.ip() {
            return Ok(v4);
        }
    }
    bail!("no IPv4 address for {host}")
}
