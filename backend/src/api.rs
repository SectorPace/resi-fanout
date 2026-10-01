//! HTTP API + static UI serving (axum).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::services::ServeDir;

use crate::models::{now_ts, PortEntry};
use crate::scheduler;
use crate::snippet;
use crate::state::AppState;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router(state: Arc<AppState>, web_root: &str) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/proxies", get(proxies))
        .route("/ports", get(ports))
        .route("/refresh", post(refresh))
        .route("/check", post(check))
        .route("/config", get(get_config).put(put_config))
        .route("/3xui/snippet", get(xui_snippet))
        .route("/vpngate", get(vpngate))
        .route("/vpngate/rebuild", post(vpngate_rebuild))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth));

    let mut app = Router::new().nest("/api", api);
    if !web_root.is_empty() {
        app = app.fallback_service(ServeDir::new(web_root));
    }
    app.with_state(state)
}

pub async fn serve(state: Arc<AppState>) -> anyhow::Result<()> {
    let cfg = state.config().await;
    let web_root = cfg.server.web_root.clone();
    if !web_root.is_empty() && !std::path::Path::new(&web_root).join("index.html").exists() {
        tracing::warn!(web_root, "web root has no index.html — UI will 404 (API still works)");
    }
    let app = router(state.clone(), &web_root);
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen).await?;
    tracing::info!(addr = %cfg.server.listen, "api/ui listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ------------------------------------------------------------------- auth

async fn auth(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let key = state.config().await.server.api_key;
    if !key.is_empty() {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {key}"))
            .unwrap_or(false);
        if !ok {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    next.run(req).await
}

// ------------------------------------------------------------------- handlers

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let map = state.proxies.read().await;
    let (total, alive, residential, with_port) = map
        .values()
        .fold((0, 0, 0, 0), |acc, p| {
            (
                acc.0 + 1,
                acc.1 + p.alive as usize,
                acc.2 + p.residential() as usize,
                acc.3 + p.local_port.is_some() as usize,
            )
        });
    drop(map);
    let vpn = state.vpn_tunnels.read().await;
    let (vpn_total, vpn_up, vpn_residential) = vpn.iter().fold(
        (0, 0, 0),
        |acc, t| (acc.0 + 1, acc.1 + (t.status == "up") as usize, acc.2 + t.residential() as usize),
    );
    drop(vpn);
    Json(json!({
        "version": VERSION,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "busy": state.busy.load(Ordering::SeqCst),
        "total": total,
        "alive": alive,
        "residential": residential,
        "ports": with_port,
        "max_ports": cfg.fanout.max_ports,
        "vpn_enabled": cfg.vpngate.enabled,
        "vpn_total": vpn_total,
        "vpn_up": vpn_up,
        "vpn_residential": vpn_residential,
        "last_refresh": *state.last_refresh.read().await,
        "next_refresh": *state.next_refresh.read().await,
        "last_check_all": *state.last_check_all.read().await,
        "sources": *state.source_status.read().await,
    }))
    .into_response()
}

#[allow(clippy::too_many_lines)]
async fn proxies(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let map = state.proxies.read().await;
    let alive_filter = params.get("alive").and_then(|v| v.parse::<bool>().ok());
    let residential = params
        .get("residential")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);
    let proto = params.get("proto").map(|s| s.to_ascii_lowercase());
    let country = params
        .get("country")
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    let q = params.get("q").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 2000))
        .unwrap_or(100);
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut items: Vec<&crate::models::ProxyInfo> = map
        .values()
        .filter(|p| {
            alive_filter.is_none_or(|a| p.alive == a)
                && (!residential || p.residential())
                && proto.as_deref().map_or(true, |pr| p.protocol.as_str() == pr)
                && (country.is_empty()
                    || p.country_code
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        == country)
                && (q.is_empty()
                    || p.key.to_ascii_lowercase().contains(&q)
                    || p.isp
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(&q))
        })
        .collect();
    let total = items.len();
    items.sort_by(|a, b| {
        b.alive
            .cmp(&a.alive)
            .then_with(|| {
                a.latency_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&b.latency_ms.unwrap_or(u64::MAX))
            })
            .then_with(|| a.key.cmp(&b.key))
    });
    let page: Vec<Value> = items
        .iter()
        .skip(offset)
        .take(limit)
        .map(|p| {
            json!({
                "key": p.key, "protocol": p.protocol.as_str(), "ip": p.ip, "port": p.port,
                "country": p.country, "country_code": p.country_code, "isp": p.isp,
                "anonymity": p.anonymity, "alive": p.alive, "latency_ms": p.latency_ms,
                "hosting": p.hosting, "anon_flag": p.anon_flag, "exit_ip": p.exit_ip,
                "last_check": p.last_check, "fails": p.fails, "local_port": p.local_port,
                "residential": p.residential(),
            })
        })
        .collect();
    Json(json!({ "total": total, "items": page })).into_response()
}

async fn ports(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "items": collect_port_entries(&state).await })).into_response()
}

async fn vpngate(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let pool = state.vpn_pool.read().await;
    let ranked = crate::vpngate::rank(&pool, &cfg.vpngate.countries, cfg.vpngate.min_speed_mbps);
    drop(pool);
    let top: Vec<Value> = ranked
        .iter()
        .take(50)
        .map(|s| {
            json!({
                "hostname": s.hostname, "ip": s.ip, "score": s.score,
                "ping_ms": s.ping_ms, "speed_mbps": s.speed_bps / 1_000_000,
                "country": s.country_long, "country_code": s.country_short,
                "sessions": s.sessions, "uptime_secs": s.uptime_secs,
                "logs_kept": s.logs_kept, "operator": s.operator,
            })
        })
        .collect();
    let tunnels: Vec<Value> = state
        .vpn_tunnels
        .read()
        .await
        .iter()
        .map(|t| {
            json!({
                "server_key": t.server_key, "hostname": t.hostname,
                "local_port": t.local_port, "status": t.status,
                "tun_ip": t.tun_ip, "attempts": t.attempts,
                "alive": t.alive, "latency_ms": t.latency_ms,
                "country": t.country, "country_code": t.country_code,
                "isp": t.isp, "hosting": t.hosting, "exit_ip": t.exit_ip,
                "residential": t.residential(),
            })
        })
        .collect();
    Json(json!({
        "enabled": cfg.vpngate.enabled,
        "pool_ts": state.vpn_pool_ts.load(Ordering::Relaxed),
        "pool_size": state.vpn_pool.read().await.len(),
        "tunnels": tunnels,
        "top": top,
    }))
    .into_response()
}

async fn vpngate_rebuild(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    if !cfg.vpngate.enabled {
        return (StatusCode::BAD_REQUEST, "vpngate disabled in config").into_response();
    }
    // clear assignments so the manager re-picks servers from the current pool
    *state.vpn_tunnels.write().await = vec![];
    state.dirty.store(true, Ordering::Relaxed);
    Json(json!({ "ok": true, "msg": "tunnels cleared, manager will re-select" })).into_response()
}

async fn refresh(State(state): State<Arc<AppState>>) -> Response {
    if state.busy.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, "a cycle is already running").into_response();
    }
    let st = state.clone();
    tokio::spawn(async move {
        scheduler::run_cycle(&st).await;
    });
    Json(json!({ "ok": true, "msg": "refresh started" })).into_response()
}

async fn check(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    if state.busy.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, "a cycle is already running").into_response();
    }
    let keys: Vec<String> = body
        .as_ref()
        .and_then(|b| b.0.get("keys"))
        .and_then(|k| k.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let st = state.clone();
    tokio::spawn(async move {
        if keys.is_empty() {
            crate::checker::check_everything(&st).await;
            *st.last_check_all.write().await = Some(now_ts());
        } else {
            crate::checker::check_all(&st, keys).await;
            st.assign_ports().await;
            let _ = st.save_state().await;
        }
        st.busy.store(false, Ordering::SeqCst);
    });
    Json(json!({ "ok": true, "msg": "check started" })).into_response()
}

async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    Json(state.config().await).into_response()
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(cfg): Json<crate::config::Config>,
) -> Response {
    if cfg.fanout.max_ports == 0 || cfg.fanout.base_port as u32 + cfg.fanout.max_ports > 65536 {
        return (StatusCode::BAD_REQUEST, "bad fanout port range").into_response();
    }
    if tokio::net::TcpListener::bind(&cfg.server.listen).await.is_err() {
        // not fatal, but worth flagging: current listener keeps the old addr
        tracing::warn!(listen = %cfg.server.listen, "new listen addr not bindable now (applies after restart)");
    }
    *state.cfg.write().await = cfg;
    if let Err(e) = state.save_config().await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save: {e}")).into_response();
    }
    state.assign_ports().await;
    Json(json!({ "ok": true })).into_response()
}

async fn xui_snippet(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let all = collect_port_entries(&state).await;
    let prefix = params
        .get("prefix")
        .cloned()
        .unwrap_or_else(|| "resi".to_string());

    let entries: Vec<PortEntry> = if let Some(csv) = params.get("ports") {
        let want: Vec<u16> = csv
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        all.into_iter().filter(|e| want.contains(&e.port)).collect()
    } else if params.get("residential").map(|v| v == "1").unwrap_or(false) {
        all.into_iter().filter(|e| e.residential).collect()
    } else {
        all
    };

    Json(snippet::build(&entries, &prefix)).into_response()
}

async fn collect_port_entries(state: &Arc<AppState>) -> Vec<PortEntry> {
    let map = state.proxies.read().await;
    let mut items: Vec<PortEntry> = map
        .values()
        .filter_map(|p| {
            p.local_port.map(|port| PortEntry {
                port,
                key: p.key.clone(),
                protocol: p.protocol.as_str().to_string(),
                country_code: p.country_code.clone(),
                latency_ms: p.latency_ms,
                residential: p.residential(),
                kind: "proxy".into(),
            })
        })
        .collect();
    drop(map);
    for t in state.vpn_tunnels.read().await.iter() {
        if t.status != "up" {
            continue;
        }
        items.push(PortEntry {
            port: t.local_port,
            key: format!("vpngate://{}", t.hostname),
            protocol: "socks(vpn)".into(),
            country_code: t.country_code.clone(),
            latency_ms: t.latency_ms,
            residential: t.residential(),
            kind: "vpngate".into(),
        });
    }
    items.sort_by_key(|e| e.port);
    items
}
