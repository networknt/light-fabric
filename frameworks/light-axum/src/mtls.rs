//! Peer identity comes only from this listener's validated TLS connection.
use axum::{
    extract::connect_info::Connected,
    serve::{IncomingStream, Listener},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{io, path::Path, sync::Arc, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use tokio_rustls::{TlsAcceptor, server::TlsStream};

const MAX_PENDING_HANDSHAKES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub address: String,
    pub certificate_file: String,
    pub private_key_file: String,
    pub client_ca_file: String,
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub fingerprint: String,
    pub san_uris: Vec<String>,
}

pub struct WorkloadListener {
    tcp: TcpListener,
    tls: TlsAcceptor,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, Peer)>>,
}

impl WorkloadListener {
    pub fn bound_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.tcp.local_addr()
    }
    pub async fn bind(config: &Config, dir: &Path) -> anyhow::Result<Self> {
        let cert_bytes = tokio::fs::read(dir.join(&config.certificate_file)).await?;
        let key_bytes = tokio::fs::read(dir.join(&config.private_key_file)).await?;
        let ca_bytes = tokio::fs::read(dir.join(&config.client_ca_file)).await?;
        let certs =
            rustls_pemfile::certs(&mut cert_bytes.as_slice()).collect::<Result<Vec<_>, _>>()?;
        let key = rustls_pemfile::private_key(&mut key_bytes.as_slice())?
            .ok_or_else(|| anyhow::anyhow!("broker TLS private key missing"))?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_bytes.as_slice()) {
            roots.add(cert?)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
        let mut tls = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?;
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        tls.max_early_data_size = 0;
        Ok(Self {
            tcp: TcpListener::bind(&config.address).await?,
            tls: TlsAcceptor::from(Arc::new(tls)),
            handshakes: JoinSet::new(),
        })
    }
}

impl Listener for WorkloadListener {
    type Io = TlsStream<TcpStream>;
    type Addr = Peer;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                completed = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    if let Some(Ok(Some(connection))) = completed {
                        return connection;
                    }
                }
                accepted = self.tcp.accept() => {
                    let Ok((tcp, _)) = accepted else {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    };
                    // Never await a peer's handshake in the TCP accept loop.
                    // Bound unauthenticated work; drop excess sockets immediately.
                    if self.handshakes.len() < MAX_PENDING_HANDSHAKES {
                        let acceptor = self.tls.clone();
                        self.handshakes.spawn(handshake(acceptor, tcp));
                    }
                }
            }
        }
    }
    fn local_addr(&self) -> io::Result<Peer> {
        Ok(Peer {
            fingerprint: String::new(),
            san_uris: Vec::new(),
        })
    }
}

// JoinSet owns these tasks and aborts pending handshakes when the listener drops.
async fn handshake(acceptor: TlsAcceptor, tcp: TcpStream) -> Option<(TlsStream<TcpStream>, Peer)> {
    let tls = tokio::time::timeout(Duration::from_secs(5), acceptor.accept(tcp))
        .await
        .ok()?
        .ok()?;
    let cert = tls.get_ref().1.peer_certificates()?.first()?;
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let san_uris = parsed
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    x509_parser::extensions::GeneralName::URI(uri) => Some(uri.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let peer = Peer {
        fingerprint: hex::encode(Sha256::digest(cert.as_ref())),
        san_uris,
    };
    Some((tls, peer))
}

impl Connected<IncomingStream<'_, WorkloadListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, WorkloadListener>) -> Self {
        stream.remote_addr().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn openssl(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("openssl")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "certificate fixture generation failed"
        );
    }
    #[tokio::test]
    async fn listener_requires_actual_client_certificate_and_ignores_forwarded_identity() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        openssl(
            path,
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "ca.key",
                "-out",
                "ca.pem",
                "-days",
                "1",
                "-subj",
                "/CN=A1 test CA",
            ],
        );
        for (name, usage, san) in [
            ("server", "serverAuth", "DNS:localhost"),
            ("client", "clientAuth", "URI:spiffe://a1/broker"),
        ] {
            let key = format!("{name}.key");
            let csr = format!("{name}.csr");
            let cert = format!("{name}.pem");
            let ext = format!("{name}.ext");
            openssl(
                path,
                &[
                    "req",
                    "-new",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-keyout",
                    &key,
                    "-out",
                    &csr,
                    "-subj",
                    "/CN=A1 test",
                ],
            );
            std::fs::write(
                path.join(&ext),
                format!(
                    "basicConstraints=CA:FALSE\nextendedKeyUsage={usage}\nsubjectAltName={san}\n"
                ),
            )
            .unwrap();
            openssl(
                path,
                &[
                    "x509",
                    "-req",
                    "-in",
                    &csr,
                    "-CA",
                    "ca.pem",
                    "-CAkey",
                    "ca.key",
                    "-CAcreateserial",
                    "-out",
                    &cert,
                    "-days",
                    "1",
                    "-extfile",
                    &ext,
                ],
            );
        }
        let listener = WorkloadListener::bind(
            &Config {
                address: "127.0.0.1:0".into(),
                certificate_file: "server.pem".into(),
                private_key_file: "server.key".into(),
                client_ca_file: "ca.pem".into(),
            },
            path,
        )
        .await
        .unwrap();
        let port = listener.tcp.local_addr().unwrap().port();
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<Peer>| async move {
                    axum::Json(
                        serde_json::json!({"fingerprint":peer.fingerprint,"san":peer.san_uris}),
                    )
                },
            ),
        );
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<Peer>(),
            )
            .await
            .unwrap();
        });
        let ca =
            reqwest::Certificate::from_pem(&std::fs::read(path.join("ca.pem")).unwrap()).unwrap();
        let no_identity = reqwest::Client::builder()
            .add_root_certificate(ca.clone())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("https://localhost:{port}/");
        assert!(
            no_identity
                .get(&url)
                .header("x-client-cert", "forged")
                .send()
                .await
                .is_err()
        );
        let pem = [
            std::fs::read(path.join("client.pem")).unwrap(),
            std::fs::read(path.join("client.key")).unwrap(),
        ]
        .concat();
        let client = reqwest::Client::builder()
            .add_root_certificate(ca)
            .identity(reqwest::Identity::from_pem(&pem).unwrap())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        // Keep silent TCP peers open ahead of an authenticated client. The old
        // serial accept loop takes five seconds per peer and times this request out.
        let mut idle = Vec::new();
        for _ in 0..4 {
            idle.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        }
        let result: serde_json::Value = client
            .get(&url)
            .header("x-client-cert", "forged")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["san"], serde_json::json!(["spiffe://a1/broker"]));
        assert_eq!(result["fingerprint"].as_str().unwrap().len(), 64);
        server.abort();
    }
}

/// Start a dedicated listener under the runtime's admission and shutdown gates.
/// Its router must still enforce its route-specific user/application policy.
pub fn serve_managed(
    name: &'static str,
    listener: WorkloadListener,
    router: axum::Router,
    context: &crate::ServerContext,
    controls: &'static [crate::ControlRoute],
) -> Result<(), light_runtime::RuntimeError> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let shutdown = cancellation.clone();
    let observed = cancellation.clone();
    let admission = context.admission.clone();
    let router = crate::transport::with_admission(router, admission.clone(), controls);
    let handle = tokio::spawn(async move {
        let result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<Peer>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await;
        if !observed.is_cancelled() {
            admission.fail();
            tracing::error!(listener = name, "workload listener exited unexpectedly");
        }
        result
    });
    let server = Arc::new(ManagedListener {
        name,
        cancellation,
        handle: std::sync::Mutex::new(Some(handle)),
    });
    if let Err(error) = context.lifecycle.register(server.clone()) {
        server.cancellation.cancel();
        if let Some(handle) = server.handle.lock().expect("listener handle").take() {
            handle.abort();
        }
        return Err(error);
    }
    Ok(())
}
struct ManagedListener {
    name: &'static str,
    cancellation: tokio_util::sync::CancellationToken,
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<io::Result<()>>>>,
}
#[async_trait::async_trait]
impl light_runtime::LifecycleParticipant for ManagedListener {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn quiesce(
        &self,
        _: &light_runtime::RuntimeConfig,
        _: &light_runtime::ShutdownContext,
    ) -> Result<(), light_runtime::RuntimeError> {
        self.cancellation.cancel();
        Ok(())
    }
    async fn shutdown(
        &self,
        _: &light_runtime::RuntimeConfig,
        context: &light_runtime::ShutdownContext,
    ) -> Result<(), light_runtime::RuntimeError> {
        self.cancellation.cancel();
        let handle = self.handle.lock().expect("listener handle").take();
        if let Some(mut handle) = handle {
            match tokio::time::timeout(context.remaining(), &mut handle).await {
                Ok(Ok(result)) => result.map_err(light_runtime::RuntimeError::Io)?,
                Ok(Err(error)) => {
                    return Err(light_runtime::RuntimeError::Config(format!(
                        "workload listener task failed: {error}"
                    )));
                }
                Err(_) => {
                    handle.abort();
                    return Err(light_runtime::RuntimeError::ShutdownDeadlineExceeded(
                        context.remaining(),
                    ));
                }
            }
        }
        Ok(())
    }
}
