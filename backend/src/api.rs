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

/// Normalise "/foo", "foo/" → "/foo" (empty stays empty).
pub fn normalize_base(p: &str) -> String {
    let t = p.trim().trim_matches('/');
    if t.is_empty() {
        String::new()
    } else {
        format!("/{t}")
    }
}

pub fn router(state: Arc<AppState>, web_root: &str, base_path: &str) -> Router {
    let base = normalize_base(base_path);
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
        .route("/xui/inbounds", get(xui_inbounds))
        .route("/xui/preview", post(xui_preview))
        .route("/xui/link", post(xui_link))
        .route("/xui/unlink", post(xui_unlink))
        .route("/ports/assign", post(ports_assign))
        .route("/ports/release", post(ports_release))
        .route("/ports/mode", post(ports_mode))
        .route("/warp", get(warp_status))
        .route("/warp/register", post(warp_register))
        .route("/warp/import", post(warp_import))
        .route("/warp/connect", post(warp_connect))
        .route("/warp/disconnect", post(warp_disconnect))
        .route("/warp/import-clash", post(warp_import_clash))
        .route("/warp/apply-clash", post(warp_apply_clash))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth));

    if base.is_empty() {
        let mut app = Router::new().nest("/api", api);
        if !web_root.is_empty() {
            app = app.fallback_service(
                ServeDir::new(web_root).append_index_html_on_directories(true),
            );
        }
        return app.with_state(state);
    }

    let mut app = Router::new().nest(&format!("{base}/api"), api);
    // 直接访问 /xxx（不带尾斜杠）时重定向到 /xxx/，
    // 否则 /xxx 与 /xxx/ 都匹配不到路由 → 404
    app = app.route(
        &base,
        get({
            let base = base.clone();
            move || {
                let base = base.clone();
                async move { axum::response::Redirect::temporary(&format!("{base}/")) }
            }
        }),
    );

    // Under a base path the static files are served by our own routes:
    // nest()'s catch-all does not match an empty remainder (so `/<base>/`
    // would 404) and does not reach a nested fallback. MapRequest strips the
    // prefix before ServeDir sees the request.
    if !web_root.is_empty() {
        let prefix = base.clone();
        let static_files = tower::util::MapRequest::new(
            ServeDir::new(web_root).append_index_html_on_directories(true),
            move |mut req: axum::extract::Request| {
                let path = req.uri().path().to_string();
                let stripped = path.strip_prefix(&prefix).unwrap_or("");
                let target = if stripped.is_empty() { "/" } else { stripped };
                match target.parse() {
                    Ok(uri) => *req.uri_mut() = uri,
                    Err(_) => *req.uri_mut() = axum::http::Uri::from_static("/"),
                }
                req
            },
        );
        app = app
            .route(&format!("{base}/"), axum::routing::get_service(static_files.clone()))
            .route(&format!("{base}/*path"), axum::routing::get_service(static_files));
    }

    let app = app
        .route(
            "/",
            get(move || async move {
                axum::response::Redirect::temporary(&format!("{base}/"))
            }),
        )
        .with_state(state);
    app
}

pub async fn serve(state: Arc<AppState>) -> anyhow::Result<()> {
    let cfg = state.config().await;
    let web_root = cfg.server.web_root.clone();
    if !web_root.is_empty() && !std::path::Path::new(&web_root).join("index.html").exists() {
        tracing::warn!(web_root, "web root has no index.html — UI will 404 (API still works)");
    }
    let base = normalize_base(&cfg.server.base_path);
    let app = router(state.clone(), &web_root, &base);
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen).await?;
    #[cfg(feature = "tls")]
    let scheme = if cfg.server.tls.enabled { "https" } else { "http" };
    #[cfg(not(feature = "tls"))]
    let scheme = "http";
    tracing::info!(
        addr = %cfg.server.listen,
        base = %base,
        scheme,
        "api/ui listening{}",
        if cfg.server.api_key.is_empty() { " (WARNING: no API key)" } else { "" }
    );

    #[cfg(feature = "tls")]
    if cfg.server.tls.enabled {
        match crate::tls::load(&cfg.server.tls).await {
            Ok(handle) => {
                crate::tls::serve(handle, listener, app, cfg.server.tls.clone()).await?;
                return Ok(());
            }
            Err(e) => {
                // A broken certificate must not take the service down (that
                // would turn into a systemd restart loop). Fall back to
                // localhost-only HTTP so the UI stays reachable, and never
                // downgrade a public bind to plaintext.
                tracing::error!(
                    error = %e,
                    "TLS 证书加载失败 —— 降级为仅本机 HTTP；修好证书后 rf restart 即可恢复 HTTPS"
                );
                let host = cfg.server.listen.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
                let port = cfg
                    .server
                    .listen
                    .rsplit_once(':')
                    .map(|(_, p)| p.parse::<u16>().unwrap_or(7654))
                    .unwrap_or(7654);
                if host == "0.0.0.0" || host == "::" || host.is_empty() {
                    // `listener` still owns <port> (bound before the TLS attempt),
                    // and on Linux a wildcard bind blocks a later loopback bind
                    // unless the first socket is released first — without this
                    // drop the fallback bind fails with EADDRINUSE and the exact
                    // restart loop this branch exists to prevent happens instead.
                    drop(listener);
                    let fallback = tokio::net::TcpListener::bind(("127.0.0.1", port))
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("bind 127.0.0.1:{port} after TLS failure: {e}")
                        })?;
                    tracing::warn!(port, "仅在 127.0.0.1 上以 HTTP 提供服务");
                    axum::serve(fallback, app).await?;
                    return Ok(());
                }
            }
        }
    }
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
        "auto_assign": cfg.fanout.auto_assign,
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

// ------------------------------------------------------------------ Cloudflare WARP

async fn warp_status(State(state): State<Arc<AppState>>) -> Response {
    let st = crate::warp::status(&state).await;
    let mut v = serde_json::to_value(st).unwrap_or(json!({}));
    if let Some(obj) = v.as_object_mut() {
        match crate::warp::xray_outbound(&state).await {
            Ok(ob) => {
                obj.insert("xray_outbound".into(), ob);
            }
            Err(_) => {}
        }
    }
    Json(v).into_response()
}

async fn warp_register(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let license = body
        .as_ref()
        .and_then(|b| b.0.get("license"))
        .and_then(|v| v.as_str())
        .map(String::from);
    match crate::warp::register(&state, license).await {
        Ok(msg) => Json(json!({ "ok": true, "msg": msg })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn warp_import(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let text = body
        .as_ref()
        .and_then(|b| b.0.get("config"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match crate::warp::import_profile(&state, text).await {
        Ok(p) => Json(json!({
            "ok": true,
            "endpoint": p.endpoint,
            "addresses": p.addresses,
            "dns": p.dns,
            "msg": "profile saved"
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn warp_connect(State(state): State<Arc<AppState>>) -> Response {
    match crate::warp::connect(&state).await {
        Ok(()) => Json(json!({ "ok": true, "msg": "warp tunnel up" })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn warp_disconnect(State(state): State<Arc<AppState>>) -> Response {
    crate::warp::disconnect(&state).await;
    Json(json!({ "ok": true, "msg": "warp tunnel down" })).into_response()
}

/// Parse a Clash/Mihomo YAML and list every masque/wireguard node with the
/// two conversions we support (wireguard profile / mihomo sidecar config).
async fn warp_import_clash(body: Option<Json<Value>>) -> Response {
    let yaml = body
        .as_ref()
        .and_then(|b| b.0.get("yaml"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if yaml.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "paste a clash/mihomo yaml").into_response();
    }
    let nodes = crate::warp::parse_clash(yaml);
    if nodes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "no masque/wireguard proxies found in that yaml",
        )
            .into_response();
    }
    let list: Vec<Value> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({
                "index": i, "name": n.name, "kind": n.kind,
                "server": n.server, "port": n.port,
                "addresses": n.addresses(), "mtu": n.mtu, "sni": n.sni,
                "public_key": n.public_key,
            })
        })
        .collect();
    Json(json!({ "ok": true, "count": nodes.len(), "nodes": list }))
        .into_response()
}

async fn warp_apply_clash(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let yaml = body
        .as_ref()
        .and_then(|b| b.0.get("yaml"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let index = body
        .as_ref()
        .and_then(|b| b.0.get("index"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let mode = body
        .as_ref()
        .and_then(|b| b.0.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("wireguard");
    match crate::warp::apply_clash(&state, yaml, index, mode).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn ports(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "items": collect_port_entries(&state).await })).into_response()
}

async fn vpngate(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let pool = state.vpn_pool.read().await;
    let ranked = crate::vpngate::rank(&pool, &cfg.vpngate.countries, cfg.vpngate.min_speed_mbps);
    let live_count = pool
        .iter()
        .filter(|s| s.last_seen > crate::models::now_ts() - 3600)
        .count();
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
                "last_seen": s.last_seen,
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
        "pool_size": live_count,
        "pool_cached": state.vpn_pool.read().await.len(),
        "meta": *state.vpn_meta.read().await,
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

// ------------------------------------------------------------ 3x-ui inbounds

async fn xui_inbounds(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let db = crate::xui::resolve_db_path(&cfg.xui);
    let args = ["list".to_string(), "--db".into(), db.clone()];
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::xui::run_script(&cfg.xui, &arg_refs).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            let msg = format!("{e}（已探测: {}）", crate::xui::probed_paths(&cfg.xui));
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
    }
}

/// Shared body for preview/link: template_id + ports (+ residential filter).
async fn xui_body(state: &Arc<AppState>, body: Option<&Json<Value>>) -> anyhow::Result<(Vec<u16>, bool, Value)> {
    let template_id = body
        .and_then(|b| b.0.get("template_id"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let residential = body
        .and_then(|b| b.0.get("residential_only"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut ports: Vec<u16> = body
        .and_then(|b| b.0.get("ports"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_u64()).filter_map(|x| u16::try_from(x).ok()).collect())
        .unwrap_or_default();
    if ports.is_empty() {
        ports = collect_port_entries(state)
            .await
            .iter()
            .filter(|e| !residential || e.residential)
            .map(|e| e.port)
            .collect();
    }
    let entries = crate::xui::entries_for(state, &ports, residential).await;
    Ok((ports, residential, json!({ "template_id": template_id, "entries": entries })))
}

async fn xui_preview(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let cfg = state.config().await;
    let (_ports, _resi, payload) = match xui_body(&state, body.as_ref()).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    };
    let template_id = payload["template_id"].as_i64().unwrap_or(0);
    let entries = payload["entries"].to_string();
    let args: Vec<String> = crate::xui::script_args(&cfg.xui, "preview", template_id, &entries);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::xui::run_script(&cfg.xui, &arg_refs).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn xui_link(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let mut cfg = state.config().await;
    if let Some(h) = body.as_ref().and_then(|b| b.0.get("host")).and_then(|v| v.as_str()) {
        if !h.is_empty() {
            cfg.xui.host = h.to_string();
        }
    }
    let (_ports, _resi, payload) = match xui_body(&state, body.as_ref()).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    };
    let template_id = payload["template_id"].as_i64().unwrap_or(0);
    let entries = payload["entries"].to_string();
    if entries == "[]" {
        return (StatusCode::BAD_REQUEST, "no fanout ports to link").into_response();
    }
    let args: Vec<String> = crate::xui::script_args(&cfg.xui, "link", template_id, &entries);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::xui::run_script(&cfg.xui, &arg_refs).await {
        Ok(mut v) => {
            let note = if cfg.xui.auto_restart {
                crate::xui::restart_xui().await
            } else {
                "auto_restart disabled".to_string()
            };
            if let Some(obj) = v.as_object_mut() {
                obj.insert("restart".into(), json!(note));
            }
            Json(v).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn xui_unlink(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let args = [
        "unlink".to_string(),
        "--db".into(),
        crate::xui::resolve_db_path(&cfg.xui),
        "--inbound-prefix".into(),
        cfg.xui.inbound_prefix.clone(),
        "--outbound-prefix".into(),
        cfg.xui.outbound_prefix.clone(),
    ];
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::xui::run_script(&cfg.xui, &arg_refs).await {
        Ok(mut v) => {
            let note = if cfg.xui.auto_restart {
                crate::xui::restart_xui().await
            } else {
                "auto_restart disabled".to_string()
            };
            if let Some(obj) = v.as_object_mut() {
                obj.insert("restart".into(), json!(note));
            }
            Json(v).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
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
    // u64 arithmetic on purpose: release builds have overflow checks off, so
    // `base_port as u32 + max_ports` wraps around and let absurd values like
    // base_port=65535 + max_ports=u32::MAX pass this guard.
    let port_end = u64::from(cfg.fanout.base_port) + u64::from(cfg.fanout.max_ports);
    if cfg.fanout.max_ports == 0 || port_end > 65536 {
        return (StatusCode::BAD_REQUEST, "bad fanout port range").into_response();
    }
    // an invalid base_path would panic while registering routes on the next
    // start, so reject it at write time
    let base = normalize_base(&cfg.server.base_path);
    if !base.is_empty()
        && !base
            .trim_start_matches('/')
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            "base_path may only contain letters, digits, - and _",
        )
            .into_response();
    }
    // Only probe when the address actually changes. The live listener already owns
    // the current address, so re-binding it would always fail and emit a
    // misleading warning on every ordinary config save.
    let current_listen = state.config().await.server.listen.clone();
    if cfg.server.listen != current_listen {
        // not fatal, but worth flagging: current listener keeps the old addr
        if let Err(e) = tokio::net::TcpListener::bind(&cfg.server.listen).await {
            tracing::warn!(listen = %cfg.server.listen, error = %e, "new listen addr not bindable now (applies after restart)");
        }
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
    let mode = params.get("mode").cloned().unwrap_or_else(|| "direct".into());
    let inbound = params.get("inbound").cloned().unwrap_or_default();

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

    Json(snippet::build_with(&entries, &prefix, &mode, &inbound)).into_response()
}

/// 手动为选中的代理节点分配本地端口（按延迟排序占用最低的空闲端口）
async fn ports_assign(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let keys: Vec<String> = body
        .as_ref()
        .and_then(|b| b.0.get("keys"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if keys.is_empty() {
        return (StatusCode::BAD_REQUEST, "keys 为空：请先在节点池勾选要开放的节点").into_response();
    }
    let cfg = state.config().await;
    if cfg.fanout.max_ports == 0 {
        return (StatusCode::BAD_REQUEST, "max_ports 为 0，请先在配置里调大").into_response();
    }
    let mut assigned = Vec::new();
    {
        let mut map = state.proxies.write().await;
        // 已占用的端口
        let mut used: Vec<u16> = map.values().filter_map(|p| p.local_port).collect();
        let mut keys = keys.clone();
        keys.sort_by_key(|k| {
            map.get(k).and_then(|p| p.latency_ms).unwrap_or(u64::MAX)
        });
        for k in keys {
            let Some(p) = map.get_mut(&k) else { continue };
            if p.local_port.is_some() {
                assigned.push(json!({"key": k, "port": p.local_port}));
                continue;
            }
            if used.len() >= cfg.fanout.max_ports as usize {
                break;
            }
            let base = cfg.fanout.base_port as u32;
            let mut port = None;
            for i in 0..cfg.fanout.max_ports as u32 {
                let cand = base + i;
                if cand > 65535 {
                    break;
                }
                let c = cand as u16;
                if !used.contains(&c) {
                    port = Some(c);
                    break;
                }
            }
            let Some(p2) = port else { break };
            used.push(p2);
            map.get_mut(&k).map(|p| p.local_port = Some(p2));
            assigned.push(json!({"key": k, "port": p2}));
        }
    }
    // 关掉自动分配，避免下一轮把手动结果覆盖
    {
        let mut c = state.cfg.write().await;
        if c.fanout.auto_assign {
            c.fanout.auto_assign = false;
            let _ = state.save_config().await;
        }
    }
    state.dirty.store(true, Ordering::Relaxed);
    Json(json!({ "ok": true, "assigned": assigned })).into_response()
}

/// 释放端口（节点保留在池中）
async fn ports_release(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let ports: Vec<u16> = body
        .as_ref()
        .and_then(|b| b.0.get("ports"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_u64()).filter_map(|x| u16::try_from(x).ok()).collect())
        .unwrap_or_default();
    let keys: Vec<String> = body
        .as_ref()
        .and_then(|b| b.0.get("keys"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut map = state.proxies.write().await;
    let mut released = 0usize;
    for p in map.values_mut() {
        let hit_port = ports.contains(&p.local_port.unwrap_or(0));
        let hit_key = keys.contains(&p.key);
        if hit_port || hit_key {
            if p.local_port.is_some() {
                released += 1;
            }
            p.local_port = None;
        }
    }
    drop(map);
    state.dirty.store(true, Ordering::Relaxed);
    Json(json!({ "ok": true, "released": released })).into_response()
}

/// 切换自动分配模式
async fn ports_mode(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let enabled = body
        .as_ref()
        .and_then(|b| b.0.get("auto"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    {
        let mut c = state.cfg.write().await;
        c.fanout.auto_assign = enabled;
    }
    if let Err(e) = state.save_config().await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save: {e}")).into_response();
    }
    if enabled {
        state.assign_ports().await;
    } else {
        // 切到手动：把此前自动铺上的端口全部收回，由用户自己勾选
        let mut map = state.proxies.write().await;
        for p in map.values_mut() {
            p.local_port = None;
        }
        drop(map);
        state.dirty.store(true, Ordering::Relaxed);
    }
    Json(json!({ "ok": true, "auto": enabled })).into_response()
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
