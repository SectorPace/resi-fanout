use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::config::SourceCfg;
use crate::models::{Protocol, ProxyInfo};

pub struct FetchOutcome {
    pub name: String,
    pub proxies: Vec<ProxyInfo>,
    pub error: Option<String>,
}

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) resi-fanout/1.0")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .expect("reqwest client")
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
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return fail(format!("body: {e}")),
    };
    let proxies = match s.kind.as_str() {
        "monosans" => parse_monosans(&text),
        "geonode" => parse_geonode(&text),
        _ => parse_text(&text, s.protocol),
    };
    FetchOutcome {
        name: s.name.clone(),
        proxies,
        error: None,
    }
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

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
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
    let arr = match v.as_array() {
        Some(a) => a.clone(),
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in &arr {
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
        Some(a) => a.clone(),
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in &arr {
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
