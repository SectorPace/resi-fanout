//! 3x-ui panel integration: list panel inbounds and link every fanout
//! port to its own panel inbound (fanout-style). The heavy lifting is in
//! scripts/xui_db.py (stdlib sqlite3, schema-adaptive for 3x-ui v2/v3);
//! this module just drives it and restarts x-ui afterwards.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::state::AppState;

pub async fn run_script(cfg: &crate::config::XuiCfg, args: &[&str]) -> anyhow::Result<Value> {
    if !std::path::Path::new(&cfg.script_path).exists() {
        anyhow::bail!(
            "{} not found — reinstall resi-fanout or set xui.script_path",
            cfg.script_path
        );
    }
    let mut cmd = Command::new("python3");
    cmd.arg(&cfg.script_path).arg(args[0]);
    for a in &args[1..] {
        cmd.arg(a);
    }
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let parsed: Value = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .unwrap_or(Value::Null);
    if !out.status.success() {
        let msg = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if stderr.is_empty() {
                    format!("exit {}", out.status)
                } else {
                    stderr
                }
            });
        anyhow::bail!("{msg}");
    }
    Ok(parsed)
}

/// Entries JSON for the python helper, built from current fanout ports.
pub async fn entries_for(
    state: &Arc<AppState>,
    ports: &[u16],
    residential_only: bool,
) -> Vec<Value> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    {
        // one pass over the pool instead of rescanning it per requested port:
        // `ports` can hold hundreds of entries and the map thousands, and the
        // guard must not be held across the loop. Only the two fields the
        // entry needs are copied, so this stays bounded by the assigned ports
        // instead of deep-cloning the pool.
        // Built with an explicit first-wins insert rather than `.collect()`.
        // `state.proxies` is a HashMap, so `values()` iterates in arbitrary
        // (RandomState) order and `collect()` keeps the LAST entry for a
        // duplicated port — whereas the linear `find()` this replaced kept the
        // FIRST. The two can therefore pick different proxies for the same
        // port, writing different country/residential values into the panel DB
        // and changing between restarts. Neither assign path can produce a
        // duplicate today (both track a `used` set), so this is latent — but a
        // hand-edited or restored state.json makes it live.
        let by_port: HashMap<u16, (Option<String>, bool)> = {
            let map = state.proxies.read().await;
            let mut by_port: HashMap<u16, (Option<String>, bool)> =
                HashMap::with_capacity(map.len());
            for p in map.values() {
                let Some(port) = p.local_port else { continue };
                by_port
                    .entry(port)
                    .or_insert_with(|| (p.country_code.clone(), p.residential()));
            }
            by_port
        };

        for port in ports {
            if !seen.insert(*port) {
                continue;
            }
            let Some((country, residential)) = by_port.get(port) else {
                continue;
            };
            if residential_only && !*residential {
                continue;
            }
            // NOTE: `port` is the LOCAL fanout port the socks outbound must use
            entries.push(json!({
                "port": port,
                "country": country.clone().unwrap_or_else(|| "xx".into()),
                "residential": residential,
                "kind": "proxy"
            }));
        }
    }
    for t in state.vpn_tunnels.read().await.iter() {
        if !ports.contains(&t.local_port) || t.status != "up" || !seen.insert(t.local_port) {
            continue;
        }
        if residential_only && !t.residential() {
            continue;
        }
        entries.push(json!({
            "port": t.local_port,
            "country": t.country_code.clone().unwrap_or_else(|| "xx".into()),
            "residential": t.residential(),
            "kind": "vpngate"
        }));
    }
    entries
}

/// Where a 3x-ui panel database commonly lives. Different install methods
/// (official deb, docker, manual tarball, source build) put it in different
/// places, so `xui.db_path` is only a first guess and this list backs it up.
///
/// Single source of truth on purpose: `resolve_db_path` searches exactly this and
/// `probed_paths` reports exactly this. They used to be two independent lists and
/// had drifted — `/etc/x-ui/db/x-ui.db` was searched but never shown to the user,
/// so the error message did not describe the search that actually ran.
const CANDIDATE_PATHS: &[&str] = &[
    "/etc/x-ui/x-ui.db",
    "/etc/x-ui/db/x-ui.db",
    "/usr/local/x-ui/x-ui.db",
    "/usr/local/x-ui/bin/x-ui.db",
    "/usr/local/x-ui/bin/db/x-ui.db",
    "/opt/x-ui/x-ui.db",
    "/opt/x-ui/db/x-ui.db",
];

#[derive(Debug, PartialEq, Eq)]
enum DbProbe {
    /// We can see it and stat it.
    Found,
    /// Something is there but we are not allowed to look — typically a
    /// root-owned `/etc/x-ui` (often 0700) with a 0600 `x-ui.db`, which is the
    /// *normal* state on a hardened host.
    ///
    /// This must be distinguished from `Missing`. `Path::exists()` collapses
    /// `PermissionDenied` into `false`, so the old code cheerfully reported
    /// "panel database not found" for a database that was sitting right there —
    /// sending the operator hunting for a file that already existed, and hiding
    /// the actual fix (grant the service user access).
    NotPermitted,
    Missing,
}

/// Classify the result of a `stat` on a panel-database path.
///
/// Split out from [`probe`] so the decision is testable directly: the bug being
/// fixed here was precisely that "could not stat" and "not there" were being
/// treated as the same thing, and manufacturing a real `EACCES` needs a
/// filesystem that honours permission bits (WSL's /mnt/e does not — it is DrvFs,
/// where ownership is always root and `chmod 000` is silently dropped).
fn classify(stat: std::io::Result<std::fs::Metadata>) -> DbProbe {
    match stat {
        Ok(_) => DbProbe::Found,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DbProbe::NotPermitted,
        Err(_) => DbProbe::Missing,
    }
}

fn probe(path: &str) -> DbProbe {
    classify(std::fs::metadata(path))
}

/// Compose the user-facing message for a failed panel-database operation.
///
/// Kept in one place so every 3x-ui route reports the same thing: what was
/// tried, and — crucially — whether the file is *missing* or merely unreadable,
/// since those need completely different fixes.
pub fn not_found_message(err: &anyhow::Error, cfg: &crate::config::XuiCfg) -> String {
    let mut msg = format!("{err}（已探测: {}）", probed_paths(cfg));
    if let Some(hint) = permission_hint(cfg) {
        msg.push('\n');
        msg.push_str(&hint);
    }
    msg
}

/// The paths we actually look at, `xui.db_path` first.
fn searched_paths(cfg: &crate::config::XuiCfg) -> Vec<String> {
    let mut all = Vec::with_capacity(CANDIDATE_PATHS.len() + 1);
    all.push(cfg.db_path.clone());
    all.extend(CANDIDATE_PATHS.iter().map(|s| s.to_string()));
    // `xui.db_path` usually *is* one of the candidates, and listing it twice made
    // the error look like the detection had picked a path and then rejected it.
    let mut seen = std::collections::HashSet::new();
    all.retain(|p| seen.insert(p.clone()));
    all
}

/// 面板数据库不一定在 /etc/x-ui/（不同安装方式位置不同），
/// 配置里指定的路径不存在时按常见位置探测，并把结果用于后续操作。
pub fn resolve_db_path(cfg: &crate::config::XuiCfg) -> String {
    if probe(&cfg.db_path) == DbProbe::Found {
        return cfg.db_path.clone();
    }
    for cand in CANDIDATE_PATHS {
        if probe(cand) == DbProbe::Found {
            return cand.to_string();
        }
    }
    cfg.db_path.clone()
}

pub fn probed_paths(cfg: &crate::config::XuiCfg) -> String {
    searched_paths(cfg).join(", ")
}

/// Extra guidance when the database is present but unreadable, which is the
/// failure mode `resolve_db_path` cannot resolve on its own.
pub fn permission_hint(cfg: &crate::config::XuiCfg) -> Option<String> {
    let blocked: Vec<String> = searched_paths(cfg)
        .into_iter()
        .filter(|p| probe(p) == DbProbe::NotPermitted)
        .collect();
    if blocked.is_empty() {
        return None;
    }
    Some(format!(
        "但以下路径存在却无法读取：{}。\
         面板数据库通常归 root 且目录权限为 0700，而本服务以 resi-fanout 运行，\
         所以它是「存在但读不到」而不是「不存在」。\
         请授予服务用户读写权限（例如把 resi-fanout 加入数据库所属组，\
         或用 setfacl -m u:resi-fanout:rw <路径>），\
         注意不要破坏面板自身运行用户（x-ui）的访问",
        blocked.join(", ")
    ))
}

pub fn script_args(
    cfg: &crate::config::XuiCfg,
    cmd: &str,
    template_id: i64,
    entries: &str,
) -> Vec<String> {
    vec![
        cmd.to_string(),
        "--db".into(),
        resolve_db_path(cfg),
        "--template-id".into(),
        template_id.to_string(),
        "--entries".into(),
        entries.to_string(),
        "--host".into(),
        cfg.host.clone(),
        "--inbound-prefix".into(),
        cfg.inbound_prefix.clone(),
        "--outbound-prefix".into(),
        cfg.outbound_prefix.clone(),
        "--inbound-port-base".into(),
        cfg.inbound_port_base.to_string(),
        "--fanout-bind".into(),
        "127.0.0.1".into(),
    ]
}

pub async fn restart_xui() -> String {
    let which = Command::new("x-ui").arg("restart").output().await;
    match which {
        Ok(o) if o.status.success() => "x-ui restarted".into(),
        _ => match Command::new("systemctl")
            .args(["restart", "x-ui"])
            .output()
            .await
        {
            Ok(o) if o.status.success() => "x-ui service restarted".into(),
            Ok(_) => "x-ui restart failed (check manually)".into(),
            Err(e) => format!("x-ui restart skipped: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(db_path: &str) -> crate::config::XuiCfg {
        crate::config::XuiCfg {
            db_path: db_path.to_string(),
            ..Default::default()
        }
    }

    /// Every path the code claims to search must actually appear in the list it
    /// reports. These two used to be independent lists that had drifted, so the
    /// error message silently omitted a path that was being searched.
    #[test]
    fn reported_paths_match_the_searched_paths() {
        let cfg = cfg_with("/etc/x-ui/x-ui.db");
        let reported = probed_paths(&cfg);
        for cand in CANDIDATE_PATHS {
            assert!(
                reported.contains(cand),
                "searched {cand} but the error message does not mention it: {reported}"
            );
        }
    }

    /// The default `db_path` equals the first candidate, so an unfiltered list
    /// showed the same path twice — which reads like "we found it and then
    /// rejected it", the exact confusion that prompted this report.
    #[test]
    fn reported_paths_are_deduplicated_in_order() {
        let cfg = cfg_with("/etc/x-ui/x-ui.db");
        let paths = searched_paths(&cfg);
        assert_eq!(
            paths.iter().filter(|p| *p == "/etc/x-ui/x-ui.db").count(),
            1,
            "duplicate entry in {paths:?}"
        );
        assert_eq!(paths[0], cfg.db_path, "db_path must be tried first");
        let mut seen = std::collections::HashSet::new();
        assert!(paths.iter().all(|p| seen.insert(p.clone())), "{paths:?}");
    }

    #[test]
    fn a_custom_db_path_is_tried_first_and_deduped() {
        let cfg = cfg_with("/srv/panel/db.sqlite");
        let paths = searched_paths(&cfg);
        assert_eq!(paths[0], "/srv/panel/db.sqlite");
        assert!(paths.contains(&"/etc/x-ui/x-ui.db".to_string()));
        let mut seen = std::collections::HashSet::new();
        assert!(paths.iter().all(|p| seen.insert(p.clone())), "{paths:?}");
    }

    /// The classification *is* the fix, so assert it directly instead of trying to
/// provoke a real `EACCES` — that needs a filesystem which honours permission
/// bits, and WSL's /mnt/e is DrvFs (ownership pinned to root, `chmod` dropped).
#[test]
    fn permission_denied_is_not_the_same_as_missing() {
        assert_eq!(
            classify(Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))),
            DbProbe::NotPermitted
        );
        assert_eq!(
            classify(Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
            DbProbe::Missing
        );
        assert_eq!(
            classify(Err(std::io::Error::from(std::io::ErrorKind::Other))),
            DbProbe::Missing
        );
        let me = std::env::current_exe().unwrap();
        assert_eq!(classify(std::fs::metadata(me).map(|m| Ok(m)).unwrap_or_else(
            |e| panic!("{e}")
        )), DbProbe::Found);
    }

    /// An existing-but-unreadable database must not be described as missing:
    /// the fix is a permission change, not finding the file.
    #[test]
    fn unreadable_paths_are_reported_separately_from_missing_ones() {
        let cfg = cfg_with("/nonexistent/definitely/not/here.db");
        assert_eq!(probe(&cfg.db_path), DbProbe::Missing);
        assert_eq!(permission_hint(&cfg), None, "nothing here is unreadable");

        // a readable file -> Found
        let existing = std::env::current_exe().unwrap();
        assert_eq!(probe(existing.to_str().unwrap()), DbProbe::Found);

        // End-to-end: a path reached *through* an unreadable directory -> NotPermitted.
        // Two preconditions, both checked rather than assumed, because CI is the
        // only place this actually runs:
        //   - a non-root uid (root bypasses permission bits), and
        //   - a filesystem that honours the mode (WSL's /mnt/e is DrvFs:
        //     ownership pinned to root, `chmod` silently dropped).
        // The probe target must be INSIDE the locked-down directory, not the
        // directory itself: stat() on an inode needs only search permission on
        // its *parent*, so `metadata(<the 000 dir>)` succeeds and would report
        // Found. Verified on a loop-mounted ext4 image:
        //   stat(<000 dir>)          -> 0                (Found)
        //   stat(<000 dir>/inner)    -> Permission denied (NotPermitted)
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::config::writable_tmpdir("xui-perm");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("inner")).unwrap();
            let inner = dir.join("inner/target.db");
            std::fs::write(&inner, b"x").unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

            let mode = std::fs::metadata(&dir)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777);
            let chmod_took = mode == Some(0);
            let denied = std::fs::metadata(&inner)
                .err()
                .map(|e| e.kind())
                .filter(|k| *k == std::io::ErrorKind::PermissionDenied);

            if chmod_took && denied.is_some() {
                let cfg2 = cfg_with(inner.to_str().unwrap());
                assert_eq!(
                    probe(inner.to_str().unwrap()),
                    DbProbe::NotPermitted,
                    "unreachable path must not look Missing"
                );
                let msg = not_found_message(&anyhow::anyhow!("boom"), &cfg2);
                assert!(msg.contains("存在却无法读取"), "{msg}");
                assert!(msg.contains("setfacl"), "hint must name a fix: {msg}");
            }
            // restore so cleanup can succeed
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn message_names_the_searched_paths() {
        let cfg = cfg_with("/nope/x-ui.db");
        let msg = not_found_message(&anyhow::anyhow!("panel database not found: /nope/x-ui.db"), &cfg);
        assert!(msg.contains("已探测"), "{msg}");
        assert!(msg.contains("/nope/x-ui.db"), "{msg}");
        assert!(!msg.contains("存在却无法读取"), "{msg}");
    }

    /// resolve_db_path must hand the script a path that actually exists.
    #[test]
    fn resolve_prefers_an_existing_configured_path() {
        let me = std::env::current_exe().unwrap();
        let cfg = cfg_with(me.to_str().unwrap());
        assert_eq!(resolve_db_path(&cfg), me.to_str().unwrap());
    }
}
