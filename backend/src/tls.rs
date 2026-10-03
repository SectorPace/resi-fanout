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
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::TlsCfg;

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

    let cert_chain: Vec<rustls_pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&cert_data)
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("parse cert {}: {e}", cfg.cert_path))?;
    if cert_chain.is_empty() {
        anyhow::bail!("no certificate found in {}", cfg.cert_path);
    }

    let key = match rustls_pemfile::private_key(&key_data) {
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
pub async fn serve(
    handle: TlsHandle,
    listener: TcpListener,
    app: axum::Router,
    cfg: TlsCfg,
) -> anyhow::Result<()> {
    let mut last = fingerprint(&cfg.cert_path);
    let mut since_check = Duration::ZERO;
    loop {
        let acceptor = handle.acceptor.read().await.clone();
        let (tcp, peer) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            match acceptor.accept(tcp).await {
                Ok(stream) => {
                    let svc = TowerToHyperService::new(app.into_make_service());
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await
                    {
                        debug!(%peer, error = %e, "tls connection ended");
                    }
                }
                Err(e) => debug!(%peer, error = %e, "tls handshake failed"),
            }
        });

        since_check += Duration::from_millis(200);
        if since_check.as_secs() >= cfg.reload_secs.max(10) {
            since_check = Duration::ZERO;
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
