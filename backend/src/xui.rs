//! 3x-ui panel integration: list panel inbounds and link every fanout
//! port to its own panel inbound (fanout-style). The heavy lifting is in
//! scripts/xui_db.py (stdlib sqlite3, schema-adaptive for 3x-ui v2/v3);
//! this module just drives it and restarts x-ui afterwards.

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
            .unwrap_or_else(|| if stderr.is_empty() { format!("exit {}", out.status) } else { stderr });
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
        let map = state.proxies.read().await;
        for port in ports {
            if !seen.insert(*port) {
                continue;
            }
            let Some(p) = map.values().find(|p| p.local_port == Some(*port)) else {
                continue;
            };
            if residential_only && !p.residential() {
                continue;
            }
            // NOTE: `port` is the LOCAL fanout port the socks outbound must use
            entries.push(json!({
                "port": port,
                "country": p.country_code.clone().unwrap_or_else(|| "xx".into()),
                "residential": p.residential(),
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

pub fn script_args(
    cfg: &crate::config::XuiCfg,
    cmd: &str,
    template_id: i64,
    entries: &str,
) -> Vec<String> {
    vec![
        cmd.to_string(),
        "--db".into(),
        cfg.db_path.clone(),
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
    let which = Command::new("x-ui")
        .arg("restart")
        .output()
        .await;
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