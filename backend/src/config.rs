use anyhow::Context;
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
    /// true = 服务按延迟自动把端口铺给最好的节点；
    /// false = 只给用户在 UI 里勾选的节点分配端口
    ///
    /// 默认 false，即**按需开端口**：装完不抢占任何端口，第一个端口都要用户
    /// 在「节点池」页勾选节点并点「为勾选节点开放端口」才会出现。之前默认 true
    /// 会在第一次抓取+检测结束后立刻铺满 `max_ports`，用户还没来得及决定要用
    /// 哪些出口，端口就已经被占满、链接也已经生成好了。
    ///
    /// 参考 byJoey/fanout 的做法：它的出口完全由用户点「新建出口」时选定的
    /// 地区和数量驱动，不会投机性地先把端口全开出来。
    ///
    /// 已有配置里这个键是显式写着的，所以升级不会改变老实例的行为；只有全新
    /// 安装（以及删掉该键后回落到默认值）才会变成按需开端口。
    pub auto_assign: bool,
}

impl Default for FanoutCfg {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            base_port: 20000,
            mode: FanoutMode::Socks,
            // keep the default modest; fanout ports are localhost-only, so
            // raise this in the UI only if you actually need more exits
            max_ports: 20,
            // 按需开端口：默认不自动铺端口，等用户在 UI 里勾选。
            auto_assign: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckerCfg {
    pub timeout_secs: u64,
    /// In-flight classifier requests.
    ///
    /// This bounds only how many checks are *outstanding at once*, not how
    /// fast a round completes: the classifier endpoint's own per-minute quota
    /// is the binding limit for proxies that actually reach it (~45 req/min for
    /// the default ip-api.com endpoint), and a 429 is deliberately treated as
    /// "learned nothing" rather than "proxy is dead", so overshooting is safe.
    /// Raising it is still worth it, because proxies that *time out* never
    /// reach the endpoint and consume no quota — at 8 s each, a pool of 4000
    /// dead entries drains in ~2 min at 256 versus ~17 min at 32.
    ///
    /// To actually classify a large pool faster, point `classify_url` at an
    /// endpoint with a higher quota.
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
    /// plain WireGuard). It listens on `mihomo_port`.
    pub mihomo_bin: String,
    pub mihomo_port: u16,
    pub mihomo_conf: String,
    /// The fanout port clients actually connect to for a MASQUE tunnel.
    ///
    /// This MUST differ from `mihomo_port`. The sidecar is the *upstream* and
    /// already binds `mihomo_port` on loopback (`mixed-port`, allow-lan false),
    /// so reusing it for our own listener made the two fight over one port:
    /// whichever lost got EADDRINUSE, and if ours won its dialer pointed at
    /// itself, so every client CONNECT re-entered the listener and dialed the
    /// same port again until fds ran out.
    pub masque_port: u16,
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
            masque_port: 22200,
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
    /// Load the config, falling back to defaults **only** when the file is
    /// genuinely absent (first run).
    ///
    /// Any other read failure must abort startup. Substituting defaults here
    /// used to fail *open*: `Config::default()` has an empty `api_key`, so a
    /// transient EACCES (e.g. `chown root:resi-fanout` having failed) silently
    /// brought the whole `/api` surface up unauthenticated, on the default
    /// port, with a `web_root` that does not resolve — and then tried to
    /// overwrite the operator's config with those defaults.
    /// Invariants that must hold for a config to be usable, checked identically
/// whether it arrived through `PUT /api/config` or was read off disk.
///
/// This used to live only inside `put_config`, which meant a hand-edited (or
/// truncated-at-a-field-boundary) config.json deserialised cleanly through
/// `#[serde(default)]` and bypassed every guard — e.g.
/// `{"fanout": {"max_ports": 99999999}}` started the service, and
/// `next_free_port` then cast out-of-range candidates to `u16`.
pub fn validate(&self) -> Result<(), String> {
    // u64 arithmetic on purpose: release builds have overflow checks off, so
    // `base_port as u32 + max_ports` wraps around and lets absurd values like
    // base_port=65535 + max_ports=u32::MAX pass.
    let port_end = u64::from(self.fanout.base_port) + u64::from(self.fanout.max_ports);
    if self.fanout.max_ports == 0 || port_end > 65536 {
        return Err(format!(
            "bad fanout port range: base_port {} + max_ports {} is not a usable block",
            self.fanout.base_port, self.fanout.max_ports
        ));
    }
    // An invalid base_path panics while registering routes on the next start, so
    // it has to be rejected before it is ever stored.
    let base = crate::api::normalize_base(&self.server.base_path);
    if !base.is_empty()
        && !base
            .trim_start_matches('/')
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("base_path may only contain letters, digits, - and _".into());
    }
    // The WireGuard profile is interpolated into a PostUp line wg-quick hands to
    // `sh -c`, so an unusable interface name must never reach managed_conf.
    if self.warp.enabled && !crate::warp::valid_iface(&self.warp.interface) {
        return Err(format!(
            "warp.interface {:?} is not a valid interface name (allowed: A-Za-z0-9_.- , max 15 chars)",
            self.warp.interface
        ));
    }
    if self.warp.enabled && self.warp.masque_port == self.warp.mihomo_port {
        return Err(format!(
            "warp.masque_port and warp.mihomo_port are both {} — the fanout listener and the \
             mihomo sidecar must not share one port",
            self.warp.mihomo_port
        ));
    }
    Ok(())
}

pub fn load(path: &str) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let c: Config = serde_json::from_str(&s)
                    .with_context(|| format!("parse config {path}"))?;
                // Serde alone cannot catch a structurally-wrong-but-valid file:
                // `#[serde(default)]` fills every absent field, so a truncated or
                // hand-edited config happily produces a working Config with an
                // absurd port range. Validate before it can reach the listeners.
                if let Err(why) = c.validate() {
                    anyhow::bail!(
                        "config {path} is invalid: {why}\n\
                         refusing to start with it. Fix the file, or delete it to regenerate \
                         defaults (which have an empty api_key — set one before exposing the port)."
                    );
                }
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // `read_to_string` reports NotFound for a *dangling symlink* too,
                // and then we would write a default config — with an empty
                // api_key — over the operator's chosen path, which is exactly the
                // fail-open this function exists to prevent. Only treat the file
                // as genuinely absent when nothing is there at all.
                if let Ok(md) = std::fs::symlink_metadata(path) {
                    if md.file_type().is_symlink() {
                        anyhow::bail!(
                            "config {path} is a dangling symlink (its target does not exist).\n\
                             refusing to write a default config over it: that default has an empty \
                             api_key, so every /api route would end up unauthenticated. Create the \
                             target file or point --config somewhere real."
                        );
                    }
                }
                let c = Config::default();
                if let Err(write_err) = c.save_to(path) {
                    tracing::warn!(path, error = %write_err, "could not write default config");
                }
                tracing::warn!(path, "no config file yet, using built-in defaults");
                Ok(c)
            }
            Err(e) => anyhow::bail!(
                "cannot read config {path}: {e}\n\
                 refusing to start with default settings, because the default api_key is empty \
                 (every /api route would be unauthenticated). Fix the file or its permissions, \
                 e.g. chown root:resi-fanout {path} && chmod 640 {path}"
            ),
        }
    }

    pub fn save_to(&self, path: &str) -> anyhow::Result<()> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let data = serde_json::to_string_pretty(self)?;
        // Write to a sibling temp file and rename, so a crash (or a full disk)
        // mid-write cannot leave a truncated config behind: a half-written file
        // fails to parse, and `load` now aborts on that instead of quietly
        // resetting the settings. The mode of the existing file is carried over
        // because install.sh hands this file to root:<app> 0640 and the default
        // 0644 would expose the api_key to every local account.
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, data.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o600);
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}

/// First writable directory among a few candidates.
///
/// `std::env::temp_dir()` is not reliably writable — WSL images in particular
/// often mount `/tmp` read-only, which would make every test that needs a
/// scratch directory fail for a reason unrelated to the code under test.
#[cfg(test)]
pub(crate) fn writable_tmpdir(tag: &str) -> std::path::PathBuf {
    let name = format!(
        "rf-{tag}-{}-{}",
        std::process::id(),
        crate::models::now_ts()
    );
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for var in ["CARGO_TARGET_TMPDIR", "CARGO_TARGET_DIR"] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                roots.push(std::path::PathBuf::from(p));
            }
        }
    }
    roots.push(std::env::temp_dir());
    for root in &roots {
        let d = root.join(&name);
        if std::fs::create_dir_all(&d).is_ok() {
            return d;
        }
    }
    panic!("no writable temp dir among {roots:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        writable_tmpdir(&format!("cfg-{tag}"))
    }

    #[test]
    fn accepts_the_shipped_defaults() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn rejects_the_invariants_the_api_also_rejects() {
        // Absurd max_ports used to deserialise cleanly through
        // `#[serde(default)]` and only be caught on the PUT path.
        let mut c = Config::default();
        c.fanout.max_ports = 99_999_999;
        assert!(c.validate().is_err(), "absurd max_ports accepted");

        // base_port + max_ports wrapping past the u16 space
        let mut c = Config::default();
        c.fanout.base_port = 65535;
        c.fanout.max_ports = 40000;
        assert!(c.validate().is_err(), "overflowing port range accepted");

        let mut c = Config::default();
        c.fanout.max_ports = 0;
        assert!(c.validate().is_err(), "max_ports=0 accepted");

        // a base_path that panics Router::route on the next start
        let mut c = Config::default();
        c.server.base_path = "/bad*path".into();
        assert!(c.validate().is_err(), "invalid base_path accepted");

        // the masque fanout listener and the mihomo sidecar must not share a port
        let mut c = Config::default();
        c.warp.enabled = true;
        c.warp.masque_port = c.warp.mihomo_port;
        assert!(c.validate().is_err(), "shared masque/mihomo port accepted");

        let mut c = Config::default();
        c.warp.enabled = true;
        c.warp.interface = "rf; id".into();
        assert!(c.validate().is_err(), "shell-unsafe interface accepted");
    }

    #[test]
    fn load_rejects_a_structurally_wrong_but_parseable_config() {
        let d = tmpdir("wrong");
        let p = d.join("config.json");
        // valid JSON, but max_ports is nonsense; `#[serde(default)]` used to
        // let this start the service.
        std::fs::write(&p, r#"{"fanout": {"max_ports": 99999999}}"#).unwrap();
        let err = Config::load(p.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("invalid"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_refuses_to_overwrite_a_dangling_symlink() {
        let d = tmpdir("dangling");
        let target = d.join("not-created-yet.json");
        let link = d.join("config.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let res = Config::load(link.to_str().unwrap());
        assert!(res.is_err(), "wrote a default config over a dangling symlink");
        // and, critically, no config was created at either path
        assert!(!target.exists(), "default config was written to the link target");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_still_bootstraps_when_the_file_is_genuinely_absent() {
        let d = tmpdir("absent");
        let p = d.join("config.json");
        let c = Config::load(p.to_str().unwrap()).expect("absent config must fall back");
        assert_eq!(c.fanout.max_ports, 20);
        assert!(p.exists(), "default config should have been written");
        let _ = std::fs::remove_dir_all(&d);
    }
}
