//! TLS termination for the API/UI, built for ACME **IP certificates**.
//!
//! Let's Encrypt issues 6-day certificates for IP addresses, so the key pair
//! is replaced every few days: we watch the files and hot-swap the acceptor,
//! without dropping connections and without restarting the service.
//!
//! axum 0.7 has no TLS support of its own, so we accept TLS sockets here and
//! hand the plaintext stream to hyper.

use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};

use crate::config::TlsCfg;

/// This listener is meant to be reachable from the internet, so a client that
/// never finishes its request must not keep a task (and a descriptor) forever.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound on connections served at once; without it a slowloris client can
/// still exhaust the process long before it exhausts the listen backlog.
const MAX_CONNECTIONS: usize = 512;

pub struct TlsHandle {
    acceptor: Arc<RwLock<Arc<tokio_rustls::TlsAcceptor>>>,
}

pub async fn load(cfg: &TlsCfg) -> anyhow::Result<TlsHandle> {
    let sc = load_server_config(cfg).await?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(sc));
    Ok(TlsHandle {
        acceptor: Arc::new(RwLock::new(Arc::new(acceptor))),
    })
}

async fn load_server_config(cfg: &TlsCfg) -> anyhow::Result<rustls::ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert_data = tokio::fs::read(&cfg.cert_path)
        .await
        .map_err(|e| anyhow::anyhow!("read cert {}: {e}", cfg.cert_path))?;
    let key_data = tokio::fs::read(&cfg.key_path)
        .await
        .map_err(|e| anyhow::anyhow!("read key {}: {e}", cfg.key_path))?;

    // rustls-pemfile 2.x reads from a BufRead, not a byte slice
    let mut cert_reader = std::io::BufReader::new(&cert_data[..]);
    let cert_chain: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("parse cert {}: {e}", cfg.cert_path))?;
    if cert_chain.is_empty() {
        anyhow::bail!("no certificate found in {}", cfg.cert_path);
    }

    let mut key_reader = std::io::BufReader::new(&key_data[..]);
    let key = match rustls_pemfile::private_key(&mut key_reader) {
        Ok(Some(k)) => k,
        Ok(None) => anyhow::bail!("no private key found in {}", cfg.key_path),
        Err(e) => anyhow::bail!("parse key {}: {e}", cfg.key_path),
    };

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| anyhow::anyhow!("invalid cert/key pair: {e}"))
}

fn fingerprint(path: &str) -> Option<(u64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((md.len(), mtime))
}

/// Accept loop with a hot-swappable TLS acceptor.
///
/// The certificate is re-checked from a real timer, not from connection
/// arrivals: counting accepted sockets meant an idle service never reloaded
/// its files, and the first connection after a renewal was still served with
/// the old certificate.
pub async fn serve(
    handle: TlsHandle,
    listener: TcpListener,
    app: axum::Router,
    cfg: TlsCfg,
) -> anyhow::Result<()> {
    let mut last = fingerprint(&cfg.cert_path);
    let period = Duration::from_secs(cfg.reload_secs.max(10));
    let mut reload = tokio::time::interval(period);
    reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, peer) = accepted?;
                let acceptor = handle.acceptor.read().await.clone();
                // the permit is held by the spawned task, so the count drops
                // back as soon as the connection ends
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    debug!(%peer, "connection limit reached, dropping");
                    continue;
                };
                let app = app.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => {
                            let svc = TowerToHyperService::new(app.clone());
                            let mut http = http1::Builder::new();
                            // an explicit timer is required: with one set, a
                            // configured read timeout is enforced instead of
                            // warned about and ignored
                            http.timer(TokioTimer::new());
                            http.header_read_timeout(HEADER_READ_TIMEOUT);
                            if let Err(e) = http.serve_connection(TokioIo::new(stream), svc).await {
                                debug!(%peer, error = %e, "tls connection ended");
                            }
                        }
                        Ok(Err(e)) => debug!(%peer, error = %e, "tls handshake failed"),
                        Err(_) => debug!(%peer, "tls handshake timed out"),
                    }
                });
            }
            _ = reload.tick() => {
                let now = fingerprint(&cfg.cert_path);
                if now != last {
                    last = now;
                    match load_server_config(&cfg).await {
                        Ok(sc) => {
                            *handle.acceptor.write().await =
                                Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(sc)));
                            info!("TLS certificate reloaded");
                        }
                        Err(e) => warn!(error = %e, "TLS reload failed, keeping the old certificate"),
                    }
                }
            }
        }
    }
}
