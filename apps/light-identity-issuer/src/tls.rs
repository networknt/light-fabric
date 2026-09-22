//! HTTPS for the issuer.
//!
//! The bootstrap call carries a long-lived app token (and the CSR), so it must
//! not cross a network in cleartext. This is server-authentication TLS only: no
//! client certificate is requested, because the very first caller has none yet.
//! Renewal is authenticated by a signature from a still-valid workload key, not by mTLS, so the
//! renewal API remains independent of listener-level client-certificate policy.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum_server::tls_rustls::RustlsConfig;

/// Load a certificate chain and its private key from PEM files.
///
/// The provider is passed explicitly, so this does not depend on a process-wide
/// default having been installed. `with_single_cert` checks that the key
/// matches the certificate, so a mismatched pair fails here, at startup, and not
/// on the first handshake.
pub fn server_config(
    certificate_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let open = |path: &Path| {
        File::open(path)
            .map(BufReader::new)
            .map_err(|e| format!("could not read {}: {e}", path.display()))
    };

    let certificates = rustls_pemfile::certs(&mut open(certificate_path)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{} is not valid PEM: {e}", certificate_path.display()))?;
    if certificates.is_empty() {
        return Err(format!(
            "{} contains no certificate",
            certificate_path.display()
        ));
    }
    let key = rustls_pemfile::private_key(&mut open(key_path)?)
        .map_err(|e| format!("{} is not valid PEM: {e}", key_path.display()))?
        .ok_or_else(|| format!("{} contains no private key", key_path.display()))?;

    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS protocol versions: {e}"))?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(|e| format!("the certificate and key do not form a usable pair: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Serve `app` over HTTPS on an already-bound listener until it fails.
pub async fn serve_tls(
    listener: std::net::TcpListener,
    app: Router,
    config: Arc<rustls::ServerConfig>,
) -> std::io::Result<()> {
    serve_tls_until(
        listener,
        app,
        config,
        std::future::pending(),
        Duration::ZERO,
    )
    .await
}

/// Serve `app` over HTTPS until it fails or `shutdown` completes. On shutdown the listener
/// stops accepting, requests in flight get up to `grace` to finish, and any connection still
/// open after that (an idle keep-alive, say) is closed, so the call always returns promptly.
pub async fn serve_tls_until(
    listener: std::net::TcpListener,
    app: Router,
    config: Arc<rustls::ServerConfig>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let handle = axum_server::Handle::new();
    let stopper = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        stopper.graceful_shutdown(Some(grace));
    });
    axum_server::from_tcp_rustls(listener, RustlsConfig::from_config(config))
        .handle(handle)
        .serve(app.into_make_service())
        .await
}

/// Resolves on SIGTERM (what `docker stop` sends) or Ctrl-C.
///
/// The service is PID 1 in its container, and the kernel gives PID 1 no default action for a
/// signal it has not installed a handler for. Without this, `docker stop` is ignored until the
/// grace period ends and the process is killed.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
