use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tracing::warn;

use crate::config::SourceCfg;
use crate::models::{Protocol, ProxyInfo};

pub struct FetchOutcome {
    pub name: String,
    pub proxies: Vec<ProxyInfo>,
    pub error: Option<String>,
}

/// Hard cap on rows accepted from a single source.
const MAX_ROWS_PER_SOURCE: usize = 20_000;
/// Hard cap on bytes read from a single source body.
const MAX_BYTES_PER_SOURCE: usize = 32 * 1024 * 1024;

/// Build the shared HTTP client.
///
/// Returns a `Result` rather than panicking: this is called from inside the
/// detached `tokio::spawn` that runs a refresh cycle, and a panic there would
/// kill the task while leaving `AppState::busy` set — wedging every
/// `/api/refresh` and `/api/check` with 409 for the life of the process.
pub fn build_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) resi-fanout/1.0")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .context("build reqwest client")
}

pub async fn fetch_all(client: &reqwest::Client, sources: &[SourceCfg]) -> Vec<FetchOutcome> {
    let mut handles = Vec::new();
    for s in sources.iter().filter(|s| s.enabled) {
        let s = s.clone();
        let cl = client.clone();
        handles.push(tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(45), fetch_one(&cl, &s)).await {
                Ok(r) => r,
                Err(_) => FetchOutcome {
                    name: s.name.clone(),
                    proxies: vec![],
                    error: Some("fetch timeout".into()),
                },
            }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

async fn fetch_one(client: &reqwest::Client, s: &SourceCfg) -> FetchOutcome {
    let fail = |e: String| FetchOutcome {
        name: s.name.clone(),
        proxies: vec![],
        error: Some(e),
    };
    let resp = match client.get(&s.url).send().await {
        Ok(r) => r,
        Err(e) => return fail(format!("request: {e}")),
    };
    if !resp.status().is_success() {
        return fail(format!("http status {}", resp.status()));
    }
    let text = match read_capped(resp).await {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let mut proxies = match s.kind.as_str() {
        "monosans" => parse_monosans(&text),
        "geonode" => parse_geonode(&text),
        _ => parse_text(&text, s.protocol),
    };
    // Validate the target of EVERY entry here, once, rather than inside each
    // parser. The JSON kinds (monosans/geonode) build `ProxyInfo` straight from
    // the response and used to skip the check entirely, so a hostile or hijacked
    // JSON source could hand us 127.0.0.1 / 10.x / 169.254.169.254 even though
    // the plain-text path filtered them. Centralising it also covers any future
    // `kind`.
    let before = proxies.len();
    proxies.retain(|p| valid_host(&p.ip));
    if proxies.len() != before {
        warn!(
            name = %s.name,
            dropped = before - proxies.len(),
            "dropped entries that are not publicly routable addresses"
        );
    }
    // a runaway or hostile list must not be able to blow up memory
    if proxies.len() > MAX_ROWS_PER_SOURCE {
        warn!(name = %s.name, got = proxies.len(), "source row cap applied");
        proxies.truncate(MAX_ROWS_PER_SOURCE);
    }
    FetchOutcome {
        name: s.name.clone(),
        proxies,
        error: None,
    }
}

/// Read a response body, refusing anything over `MAX_BYTES_PER_SOURCE`.
///
/// `Response::text()` buffers the whole body before any limit applies, so a
/// source serving a huge (or effectively endless, within the client timeout)
/// payload would allocate until the process died — the row cap below only
/// limits what we keep, not what we read. Reject on the declared length first,
/// then enforce the ceiling while streaming.
async fn read_capped(resp: reqwest::Response) -> Result<String, String> {
    if let Some(len) = resp.content_length() {
        if len > MAX_BYTES_PER_SOURCE as u64 {
            return Err(format!("body too large: {len} bytes"));
        }
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("body: {e}"))? {
        if buf.len() + chunk.len() > MAX_BYTES_PER_SOURCE {
            return Err(format!("body exceeds {MAX_BYTES_PER_SOURCE} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Lines like "1.2.3.4:8080" or "socks5://1.2.3.4:1080 [annotations]".
fn parse_text(text: &str, default_proto: Option<Protocol>) -> Vec<ProxyInfo> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(p) = parse_line(line, default_proto) {
            if seen.insert(p.key.clone()) {
                out.push(p);
            }
        }
    }
    out
}

fn parse_line(line: &str, default_proto: Option<Protocol>) -> Option<ProxyInfo> {
    let mut proto = default_proto;
    let mut hostport = line;
    if let Some(idx) = line.find("://") {
        proto = Protocol::parse(&line[..idx]);
        hostport = &line[idx + 3..];
    }
    // strip trailing annotations (spaces / tabs / comma lists)
    let hostport = hostport
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("");
    let (ip, port) = hostport.rsplit_once(':')?;
    let ip = ip.trim().trim_start_matches('[').trim_end_matches(']').to_string();
    let port: u16 = port.trim().parse().ok()?;
    if !valid_host(&ip) {
        return None;
    }
    let proto = proto.unwrap_or(Protocol::Http);
    Some(ProxyInfo {
        key: format!("{}://{}:{}", proto.as_str(), ip, port),
        protocol: proto,
        ip,
        port,
        ..Default::default()
    })
}

/// Reject targets we must never dial or fan out.
///
/// Proxy lists are third-party data, and every accepted entry becomes both a TCP
/// connect target and — through the fanout listener — a relay that forwards
/// arbitrary client traffic to it. Without this check a hostile list (or a
/// compromised mirror) could point entries at the host's loopback, the LAN, or
/// a cloud metadata endpoint such as 169.254.169.254, turning the service into
/// an SSRF pivot.
///
/// Only a literal, globally routable address qualifies. A *hostname* is
/// rejected rather than resolved later: the list author chooses the name, so
/// they also choose where it points, which is the same SSRF by another route.
fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => is_public_unicast(ip),
        Err(_) => false, // not a literal address -> not dialable by us
    }
}

/// True only for globally routable unicast addresses.
fn is_public_unicast(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()                 // 127/8
                || v4.is_private()               // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()            // 169.254/16 (incl. 169.254.169.254)
                || v4.is_unspecified()           // 0.0.0.0
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()         // 192.0.2/24, 198.51.100/24, 203.0.113/24
                || o[0] == 0                     // 0.0.0.0/8 "this network"
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // 100.64/10 CGNAT
                || o[0] >= 240)                  // 240/4 reserved
        }
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d (IPv4-mapped) and ::a.b.c.d (IPv4-compatible) are
            // IPv4 addresses wearing a v6 hat, so they must be judged by the v4
            // rules — otherwise ::ffff:127.0.0.1 walks straight past the loopback
            // check above.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return is_public_unicast(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00    // fc00::/7 unique-local
                || (s[0] & 0xffc0) == 0xfe80)   // fe80::/10 link-local
        }
    }
}

fn jstr<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn jnum(v: &Value, key: &str) -> Option<u16> {
    v.get(key).and_then(|x| x.as_u64()).and_then(|x| u16::try_from(x).ok())
}

/// monosans/proxy-list proxies.json. Schema drifted over the years:
///   old: {"ip":..,"port":..,"protocol":..,"country":..,"anonymity":..,"isp":..}
///   new: {"host":..,"port":..,"protocol":..,"exit_ip":..,
///         "geolocation":{"country":{"iso_code":..,"names":{"en":..}}},
///         "asn":{"autonomous_system_organization":..}}
/// Parse leniently so both keep working.
fn parse_monosans(text: &str) -> Vec<ProxyInfo> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    // Borrow rather than clone: `v` is owned here and dead after this, and the
    // clone doubled peak memory for the largest input path — with a 32 MiB
    // per-source cap, a hostile or hijacked source could already force a ~32 MiB
    // Value tree, and the caps in fetch_one only run after this returns.
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in arr.iter() {
        let Some(ip) = jstr(e, "ip").or_else(|| jstr(e, "host")) else {
            continue;
        };
        let Some(port) = jnum(e, "port") else { continue };
        let Some(proto) = jstr(e, "protocol").and_then(Protocol::parse) else {
            continue;
        };
        let isp = match e.get("isp") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(obj @ Value::Object(_)) => obj
                .get("name")
                .or_else(|| obj.get("org"))
                .and_then(|x| x.as_str())
                .map(String::from),
            _ => None,
        }
        .or_else(|| {
            e.get("asn")
                .and_then(|a| a.get("autonomous_system_organization"))
                .and_then(|x| x.as_str())
                .map(String::from)
        });
        let anonymity = jstr(e, "anonymity").map(String::from);
        let country_code = jstr(e, "country_code")
            .map(String::from)
            .or_else(|| {
                jstr(e, "country").and_then(|c| {
                    if c.len() == 2 && c.chars().all(|x| x.is_ascii_alphabetic()) {
                        Some(c.to_uppercase())
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                e.get("geolocation")
                    .and_then(|g| g.get("country"))
                    .and_then(|c| c.get("iso_code"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_uppercase())
            });
        let country = jstr(e, "country")
            .filter(|c| c.len() > 2)
            .map(String::from)
            .or_else(|| {
                e.get("geolocation")
                    .and_then(|g| g.get("country"))
                    .and_then(|c| c.get("names"))
                    .and_then(|n| n.get("en"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            });
        let key = format!("{}://{}:{}", proto.as_str(), ip, port);
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(ProxyInfo {
            key,
            protocol: proto,
            ip: ip.to_string(),
            port,
            country,
            country_code,
            anonymity,
            isp,
            ..Default::default()
        });
    }
    out
}

/// geonode API: {"data":[{"ip":..,"port":..,"protocols":["http"],"anonymityLevel":
/// "elite","isp":..,"org":..,"country":..,"city":..}, ...]}
fn parse_geonode(text: &str) -> Vec<ProxyInfo> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in arr.iter() {
        let Some(ip) = jstr(e, "ip") else { continue };
        let Some(port) = jnum(e, "port") else { continue };
        let proto = e
            .get("protocols")
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.as_str())
            .and_then(Protocol::parse)
            .unwrap_or(Protocol::Http);
        let isp = jstr(e, "isp")
            .map(String::from)
            .or_else(|| jstr(e, "org").map(String::from));
        let key = format!("{}://{}:{}", proto.as_str(), ip, port);
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(ProxyInfo {
            key,
            protocol: proto,
            ip: ip.to_string(),
            port,
            country: jstr(e, "country").map(String::from),
            country_code: jstr(e, "country").and_then(|c| {
                if c.len() == 2 && c.chars().all(|x| x.is_ascii_alphabetic()) {
                    Some(c.to_uppercase())
                } else {
                    None
                }
            }),
            anonymity: jstr(e, "anonymityLevel").map(String::from),
            isp,
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_public_addresses() {
        for h in [
            "1.1.1.1",
            "8.8.8.8",
            "185.220.101.1",
            "2606:4700:4700::1111",
        ] {
            assert!(valid_host(h), "rejected a public address: {h}");
        }
    }

    #[test]
    fn rejects_internal_and_metadata_targets() {
        for h in [
            "127.0.0.1",      // loopback
            "127.1.2.3",      // whole 127/8
            "10.0.0.5",       // rfc1918
            "172.16.9.9",     // rfc1918
            "192.168.1.1",    // rfc1918
            "169.254.169.254", // cloud metadata (link-local)
            "0.0.0.0",        // unspecified
            "0.1.2.3",        // 0/8
            "100.64.0.1",     // CGNAT
            "224.0.0.1",      // multicast
            "255.255.255.255", // broadcast
            "192.0.2.5",      // documentation
            "240.0.0.1",      // reserved
            "::1",            // v6 loopback
            "fe80::1",        // v6 link-local
            "fc00::1",        // v6 unique-local
            "::",             // v6 unspecified
            "ff02::1",        // v6 multicast
        ] {
            assert!(!valid_host(h), "accepted an internal target: {h}");
        }
    }

    #[test]
    fn rejects_ipv4_addresses_disguised_as_ipv6() {
        // These are ordinary IPv4 addresses, so the v4 rules must decide.
        for h in ["::ffff:127.0.0.1", "::ffff:169.254.169.254", "::ffff:10.0.0.1"] {
            assert!(!valid_host(h), "accepted a v4-mapped internal target: {h}");
        }
        // ...and a mapped public address is still fine
        assert!(valid_host("::ffff:1.1.1.1"));
    }

    #[test]
    fn rejects_hostnames_so_dns_cannot_reach_inside() {
        // A list author who picks the name also picks where it resolves.
        for h in [
            "localhost",
            "127.0.0.1.nip.io",
            "metadata.google.internal",
            "evil.example.com",
            "anything.local",
        ] {
            assert!(!valid_host(h), "accepted a hostname: {h}");
        }
    }

    #[test]
    fn text_parsing_drops_internal_targets() {
        assert!(parse_line("1.1.1.1:8080", None).is_some());
        assert!(parse_line("socks5://127.0.0.1:1080", None).is_none());
        assert!(parse_line("10.0.0.1:3128", None).is_none());
        assert!(parse_line("169.254.169.254:80", None).is_none());
    }
}
