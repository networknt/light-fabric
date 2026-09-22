//! The HTTP client the CLI uses everywhere: HTTPS, trusting the CA bundle from `startup.yml`
//! (as well as the system roots) so the dev stack's own CA verifies.
//!
//! The CLI presents no client certificate, and to the Gateway and light-oauth no application
//! credential: it is an open, downloadable program that cannot keep a secret, so what identifies
//! those calls is the user's token.

use std::path::Path;
use std::time::Duration;

use crate::error::CliError;

pub fn client(
    ca_bundle: Option<&Path>,
    connect_timeout: Duration,
    timeout: Duration,
) -> Result<reqwest::Client, CliError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        // This client posts device codes and refresh/revocation tokens. Never let a 307/308
        // redirect replay those form bodies to a different endpoint or authority.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(path) = ca_bundle {
        let pem = std::fs::read(path).map_err(|e| {
            CliError::Config(format!(
                "could not read bootstrapCaCertPath {}: {e}",
                path.display()
            ))
        })?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
            CliError::Config(format!(
                "{} is not a PEM certificate bundle: {e}",
                path.display()
            ))
        })?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder
        .build()
        .map_err(|e| CliError::Failed(format!("could not build an HTTP client: {e}")))
}

/// TLS settings for a WebSocket: the public web roots, plus the CA bundle from `startup.yml`
/// (the dev stack's own CA), and no client certificate.
pub fn websocket_tls(
    ca_bundle: Option<&Path>,
) -> Result<std::sync::Arc<rustls::ClientConfig>, CliError> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_bundle {
        let pem = std::fs::read(path).map_err(|e| {
            CliError::Config(format!(
                "could not read bootstrapCaCertPath {}: {e}",
                path.display()
            ))
        })?;
        let mut added = 0;
        for certificate in CertificateDer::pem_slice_iter(&pem) {
            let certificate = certificate.map_err(|e| {
                CliError::Config(format!(
                    "{} is not a PEM certificate bundle: {e}",
                    path.display()
                ))
            })?;
            roots.add(certificate).map_err(|e| {
                CliError::Config(format!(
                    "{} holds an unusable certificate: {e}",
                    path.display()
                ))
            })?;
            added += 1;
        }
        if added == 0 {
            return Err(CliError::Config(format!(
                "{} holds no certificate",
                path.display()
            )));
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| CliError::Failed(format!("TLS setup failed: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
}
