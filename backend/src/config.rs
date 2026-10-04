use serde::{Deserialize, Serialize};

use crate::models::Protocol;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FanoutMode {
    #[default]
    Socks,
    Http,
    Mixed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsCfg {
    pub enabled: bool,
    /// PEM chain (fullchain.pem)
    pub cert_path: String,
    /// PEM private key (privkey.pem)
    pub key_path: String,
    /// how often to check whether the files changed (short-lived IP certs
    /// are reissued every few days, so reload without a restart)
    pub reload_secs: u64,
}

impl Default for TlsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_path: "/etc/resi-fanout/tls/fullchain.pem".into(),
            key_path: "/etc/resi-fanout/tls/privkey.pem".into(),
            reload_secs: 300,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    /// API + web UI listen address. Keep 127.0.0.1 unless you protect it.
    pub listen: String,
    /// When non-empty, every /api call needs "Authorization: Bearer <key>".
    pub api_key: String,
    /// Directory with the built frontend (index.html). Empty string disables UI.
    pub web_root: String,
    /// Random URL prefix, e.g. "/Kf3x9". Empty = serve at the root.
    pub base_path: String,
    pub tls: TlsCfg,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7654".into(),
            api_key: String::new(),
            web_root: "web".into(),
            base_path: String::new(),
            tls: TlsCfg::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FanoutCfg {
    /// Local bind for the fanout ports (what Xray connects to).
    pub bind: String,
    /// First local port; each healthy proxy gets base_port..base_port+max_ports.
    pub base_port: u16,
    /// Protocol spoken on the local ports: socks | http | mixed.
    pub mode: FanoutMode,
    pub max_ports: u32,
}

impl Default for FanoutCfg {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            base_port: 20000,
            mode: FanoutMode::Socks,
            max_ports: 100,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckerCfg {
    pub timeout_secs: u64,
    pub concurrency: usize,
    /// Upper bound on stored candidates (memory bound); worst entries evicted.
    pub max_pool: usize,
    /// Plain-HTTP endpoint queried *through* each proxy to verify liveness,
    /// get the exit IP and read the hosting flag (residential detection).
    pub classify_url: String,
}

impl Default for CheckerCfg {
    fn default() -> Self {
        Self {
            timeout_secs: 8,
            concurrency: 256,
            max_pool: 4000,
            classify_url: "http://ip-api.com/json/?fields=status,message,country,countryCode,isp,org,as,hosting,proxy,query".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerCfg {
    /// Re-fetch proxy lists every N minutes (0 disables the scheduler).
    pub refresh_minutes: u64,
    /// Re-check health of the whole pool every N minutes.
    pub recheck_minutes: u64,
    /// Drop entries that have been dead for more than N days.
    pub prune_days: u64,
}

impl Default for SchedulerCfg {
    fn default() -> Self {
        Self {
            refresh_minutes: 30,
            recheck_minutes: 20,
            prune_days: 7,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterCfg {
    /// Only fan out proxies classified as residential (hosting == false).
    pub only_residential: bool,
    /// Allow-list of ISO country codes, e.g. ["US","JP"]. Empty = all.
    pub countries: Vec<String>,
    /// Allow-list of protocols: http / socks4 / socks5. Empty = all.
    pub protocols: Vec<String>,
}

impl Default for FilterCfg {
    fn default() -> Self {
        Self {
            only_residential: false,
            countries: vec![],
            protocols: vec![],
        }
    }
}

/// VPN Gate (public SoftEther relays) — OpenVPN sidecar tunnels.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VpngateCfg {
    pub enabled: bool,
    /// Local SOCKS port for the first tunnel; +1 per server.
    pub base_port: u16,
    pub max_servers: u32,
    /// ISO country allow-list, e.g. ["JP","US","KR"]. Empty = best anywhere.
    pub countries: Vec<String>,
    /// Ignore servers below this advertised speed.
    pub min_speed_mbps: u64,
    pub openvpn_bin: String,
    /// Tear down tunnels whose exit IP classifies as datacenter.
    pub only_residential: bool,
    pub api_url: String,
    /// Directory holding vpn-up.sh / vpn-down.sh.
    pub scripts_dir: String,
    /// The official API only reports ~100 live relays; everything ever seen
    /// is cached so churned nodes can be retried later.
    pub cache_days: u64,
    pub max_pool: usize,
    /// Snapshot mirrors tried when the official API is unreachable or blocked
    /// (e.g. https://<user>.github.io/<repo>/vpngate.csv).
    pub mirror_urls: Vec<String>,
    /// Extra sources of raw OpenVPN configs (VPN Gate mirrors and the like).
    /// Each URL may serve one .ovpn or many config blocks back to back.
    pub extra_urls: Vec<String>,
}

impl Default for VpngateCfg {
    fn default() -> Self {
        Self {
            // openvpn is installed by default, so VPN Gate tunnels start
            // working out of the box; flip to false in config.json if unwanted
            enabled: true,
            base_port: 21000,
            max_servers: 3,
            countries: vec![],
            min_speed_mbps: 5,
            openvpn_bin: "openvpn".into(),
            only_residential: false,
            api_url: "https://www.vpngate.net/api/iphone/".into(),
            scripts_dir: "scripts".into(),
            cache_days: 30,
            max_pool: 800,
            mirror_urls: vec![],
            extra_urls: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceCfg {
    pub name: String,
    /// text | monosans | geonode
    pub kind: String,
    pub url: String,
    /// Default protocol for plain "ip:port" text sources.
    pub protocol: Option<Protocol>,
    pub enabled: bool,
}

impl Default for SourceCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: "text".into(),
            url: String::new(),
            protocol: None,
            enabled: true,
        }
    }
}

/// 3x-ui panel integration (fanout-style inbound takeover).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct XuiCfg {
    /// panel database
    pub db_path: String,
    /// path to xui_db.py (installed next to the binary's scripts/)
    pub script_path: String,
    /// first port used for the per-exit inbounds created in the panel
    pub inbound_port_base: u16,
    pub inbound_prefix: String,
    pub outbound_prefix: String,
    /// host used when building client links
    pub host: String,
    /// restart x-ui after writing the database
    pub auto_restart: bool,
}

impl Default for XuiCfg {
    fn default() -> Self {
        Self {
            db_path: "/etc/x-ui/x-ui.db".into(),
            script_path: "/opt/resi-fanout/scripts/xui_db.py".into(),
            inbound_port_base: 31000,
            inbound_prefix: "resi-in-".into(),
            outbound_prefix: "resi".into(),
            host: "127.0.0.1".into(),
            auto_restart: true,
        }
    }
}

/// Cloudflare WARP (WireGuard) as an extra exit source.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WarpCfg {
    pub enabled: bool,
    /// WireGuard profile (wgcf output or a pasted config). Absolute path.
    pub conf_path: String,
    /// managed interface name
    pub interface: String,
    /// local SOCKS port for the tunnel
    pub local_port: u16,
    /// launch `wgcf register` when no profile exists yet
    pub auto_register: bool,
    /// WARP+ license (passed to `wgcf register --license <key>`)
    pub license: String,
    pub keepalive: u64,
    pub mtu: u64,
    /// Mihomo sidecar (used for MASQUE nodes Clash-style, which are not
    /// plain WireGuard). Its mixed-port becomes another fanout port.
    pub mihomo_bin: String,
    pub mihomo_port: u16,
    pub mihomo_conf: String,
}

impl Default for WarpCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            conf_path: "/var/lib/resi-fanout/warp/warp.conf".into(),
            interface: "warp-rf".into(),
            local_port: 22000,
            auto_register: true,
            license: String::new(),
            keepalive: 60,
            mtu: 1280,
            mihomo_bin: "mihomo".into(),
            mihomo_port: 22100,
            mihomo_conf: "/var/lib/resi-fanout/masque/mihomo.yaml".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub fanout: FanoutCfg,
    pub checker: CheckerCfg,
    pub scheduler: SchedulerCfg,
    pub filter: FilterCfg,
    pub vpngate: VpngateCfg,
    pub warp: WarpCfg,
    pub xui: XuiCfg,
    pub sources: Vec<SourceCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            fanout: FanoutCfg::default(),
            checker: CheckerCfg::default(),
            scheduler: SchedulerCfg::default(),
            filter: FilterCfg::default(),
            vpngate: VpngateCfg::default(),
            warp: WarpCfg::default(),
            xui: XuiCfg::default(),
            sources: default_sources(),
        }
    }
}

fn default_sources() -> Vec<SourceCfg> {
    let s = |name: &str, kind: &str, url: &str, proto: Option<Protocol>| SourceCfg {
        name: name.into(),
        kind: kind.into(),
        url: url.into(),
        protocol: proto,
        enabled: true,
    };
    vec![
        s("monosans", "monosans",
          "https://raw.githubusercontent.com/monosans/proxy-list/main/proxies.json", None),
        s("thespeedx-http", "text",
          "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/http.txt", Some(Protocol::Http)),
        s("thespeedx-socks4", "text",
          "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/socks4.txt", Some(Protocol::Socks4)),
        s("thespeedx-socks5", "text",
          "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/socks5.txt", Some(Protocol::Socks5)),
        s("hideip-http", "text",
          "https://raw.githubusercontent.com/zloi-user/hideip.me/main/http.txt", Some(Protocol::Http)),
        s("hideip-socks4", "text",
          "https://raw.githubusercontent.com/zloi-user/hideip.me/main/socks4.txt", Some(Protocol::Socks4)),
        s("hideip-socks5", "text",
          "https://raw.githubusercontent.com/zloi-user/hideip.me/main/socks5.txt", Some(Protocol::Socks5)),
        s("proxifly", "text",
          "https://cdn.jsdelivr.net/gh/proxifly/free-proxy-list@main/proxies/all/data.txt", None),
        s("geonode", "geonode",
          "https://proxylist.geonode.com/api/proxy-list?limit=500&page=1&sort_by=lastChecked&sort_type=desc", None),
    ]
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(_) => {
                let c = Config::default();
                let _ = c.save_to(path);
                Ok(c)
            }
        }
    }

    pub fn save_to(&self, path: &str) -> anyhow::Result<()> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}
