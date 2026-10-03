//! resi-fanout — fetch free/residential proxies, verify them, fan each one
//! out to a local port, hand the ports to 3x-ui as Xray outbounds.

mod api;
mod checker;
mod config;
mod models;
mod openvpn;
mod relay;
mod scheduler;
mod snippet;
mod sources;
mod state;
mod vpngate;
mod warp;
mod xui;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Context};

const USAGE: &str = "\
resi-fanout — residential proxy fanout for 3x-ui

USAGE:
  resi-fanout serve                  run api + scheduler + fanout listeners (default)
  resi-fanout refresh                run one fetch+check cycle and exit
  resi-fanout check                  re-check the whole pool and exit
  resi-fanout version                print version

FLAGS:
  --config <path>   config file   (default $RESI_FANOUT_CONFIG or ./config.json)
  --data <dir>      data dir      (default $RESI_FANOUT_DATA or .)
  --web <dir>       override frontend web root
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut cmd = "serve".to_string();
    let mut cfg_path = std::env::var("RESI_FANOUT_CONFIG").unwrap_or_else(|_| "config.json".into());
    let mut data_dir = std::env::var("RESI_FANOUT_DATA").unwrap_or_else(|_| ".".into());
    let mut web_override: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "serve" | "refresh" | "check" | "version" | "--version" | "-V" => {
                cmd = args[i].trim_start_matches('-').to_string();
                if args[i] == "-V" {
                    cmd = "version".into();
                }
            }
            "--config" => {
                i += 1;
                cfg_path = args.get(i).context("--config needs a value")?.clone();
            }
            "--data" => {
                i += 1;
                data_dir = args.get(i).context("--data needs a value")?.clone();
            }
            "--web" => {
                i += 1;
                web_override = Some(args.get(i).context("--web needs a value")?.clone());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument: {other}\n{USAGE}"),
        }
        i += 1;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    if cmd == "version" {
        println!("resi-fanout {}", api::VERSION);
        return Ok(());
    }

    let cfg = config::Config::load(&cfg_path)
        .with_context(|| format!("load config {cfg_path}"))?;
    let mut cfg = cfg;
    if let Some(w) = web_override {
        cfg.server.web_root = w;
    }
    let state = Arc::new(state::AppState::new(cfg_path, PathBuf::from(data_dir), cfg));
    state.load_state().await;

    match cmd.as_str() {
        "serve" => {
            relay::spawn_supervisor(state.clone());
            openvpn::spawn_supervisor(state.clone());
            warp::supervisor(state.clone());
            scheduler::spawn(state.clone());
            api::serve(state).await?;
        }
        "refresh" => {
            scheduler::run_cycle(&state).await;
            let n = state.proxies.read().await.len();
            println!("done, pool size {n}");
        }
        "check" => {
            checker::check_everything(&state).await;
            *state.last_check_all.write().await = Some(models::now_ts());
            state.dirty.store(false, Ordering::Relaxed);
            println!("done");
        }
        other => bail!("unknown command: {other}\n{USAGE}"),
    }
    Ok(())
}
