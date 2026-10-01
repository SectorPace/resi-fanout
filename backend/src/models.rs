use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Http,
    Socks4,
    Socks5,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Socks4 => "socks4",
            Protocol::Socks5 => "socks5",
        }
    }

    pub fn parse(s: &str) -> Option<Protocol> {
        match s.to_ascii_lowercase().as_str() {
            "http" | "https" => Some(Protocol::Http),
            "socks4" | "socks4a" => Some(Protocol::Socks4),
            "socks5" | "socks5h" => Some(Protocol::Socks5),
            _ => None,
        }
    }
}

/// One upstream proxy candidate plus its runtime state.
/// `key` is the unique id: "<proto>://<ip>:<port>".
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProxyInfo {
    pub key: String,
    pub protocol: Protocol,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub isp: Option<String>,
    #[serde(default)]
    pub anonymity: Option<String>,
    // runtime
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// ip-api "hosting" flag: Some(false) => likely residential / ISP line.
    #[serde(default)]
    pub hosting: Option<bool>,
    /// ip-api "proxy" flag: exit IP is a known anonymizer.
    #[serde(default)]
    pub anon_flag: Option<bool>,
    #[serde(default)]
    pub exit_ip: Option<String>,
    #[serde(default)]
    pub last_check: Option<i64>,
    #[serde(default)]
    pub fails: u32,
    #[serde(default)]
    pub local_port: Option<u16>,
}

impl ProxyInfo {
    pub fn residential(&self) -> bool {
        self.alive && self.hosting == Some(false)
    }
}

/// A proxy currently bound to a local fanout port (returned by /api/ports
/// and consumed by the 3x-ui snippet generator).
#[derive(Clone, Debug, Serialize)]
pub struct PortEntry {
    pub port: u16,
    pub key: String,
    pub protocol: String,
    pub country_code: Option<String>,
    pub latency_ms: Option<u64>,
    pub residential: bool,
    /// "proxy" (free/paid list node) or "vpngate" (openvpn tunnel)
    pub kind: String,
}

/// One VPN Gate relay server, as parsed from the public CSV API.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct VpnServer {
    pub hostname: String,
    pub ip: String,
    #[serde(default)]
    pub remote_port: u16,
    #[serde(default)]
    pub proto: String,
    #[serde(default)]
    pub score: i64,
    #[serde(default)]
    pub ping_ms: u64,
    #[serde(default)]
    pub speed_bps: u64,
    #[serde(default)]
    pub country_long: Option<String>,
    #[serde(default)]
    pub country_short: Option<String>,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub uptime_secs: u64,
    /// false = server claims it does not keep activity logs
    #[serde(default)]
    pub logs_kept: Option<bool>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub config_b64: String,
}

impl VpnServer {
    pub fn server_key(&self) -> String {
        format!("{}|{}", self.ip, self.remote_port)
    }
}

/// Runtime state of one spawned OpenVPN sidecar tunnel.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct VpnTunnel {
    pub server_key: String,
    pub hostname: String,
    pub local_port: u16,
    #[serde(default)]
    pub status: String, // spawning | up | down | failed | rotated
    #[serde(default)]
    pub tun_ip: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    // classification (queried through the tunnel itself)
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub isp: Option<String>,
    #[serde(default)]
    pub hosting: Option<bool>,
    #[serde(default)]
    pub exit_ip: Option<String>,
    #[serde(default)]
    pub last_check: Option<i64>,
}

impl VpnTunnel {
    pub fn residential(&self) -> bool {
        self.alive && self.hosting == Some(false)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SourceStatus {
    pub name: String,
    pub ok: bool,
    pub count: usize,
    pub error: Option<String>,
    pub ts: i64,
}

pub fn split_key(key: &str) -> anyhow::Result<(Protocol, String, u16)> {
    let (scheme, rest) = key
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("bad key: {key}"))?;
    let proto = Protocol::parse(scheme).ok_or_else(|| anyhow::anyhow!("bad scheme: {scheme}"))?;
    let (ip, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("bad key: {key}"))?;
    Ok((proto, ip.to_string(), port.parse()?))
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
