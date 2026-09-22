//! Helpers shared by the integration test crates. Each crate uses a subset.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use light_cli::config::CliConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A CA, and a server certificate for `localhost`/`127.0.0.1` signed by it, written to `dir`.
/// Returns `(ca, certificate, key)` paths.
pub fn write_server_pki(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    use rcgen::{
        BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
    };
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    let server_cert = params.signed_by(&server_key, &issuer).unwrap();

    let (ca, cert, key) = (
        dir.join("ca.pem"),
        dir.join("cert.pem"),
        dir.join("key.pem"),
    );
    std::fs::write(&ca, ca_cert.pem()).unwrap();
    std::fs::write(&cert, server_cert.pem()).unwrap();
    std::fs::write(&key, server_key.serialize_pem()).unwrap();
    (ca, cert, key)
}

/// Write `startup.yml` and a `cli.yml` with the given URLs as the defaults of its placeholders,
/// side by side, and load them exactly as the binary does. The config server is a closed port and
/// the config token is `must-never-be-used` outside the config server: a test that sees it
/// anywhere else has found a leak.
pub fn config(
    dir: &TempDir,
    home: &str,
    env: &str,
    ca: &Path,
    gateway_uri: &str,
    oauth_uri: &str,
) -> CliConfig {
    config_with_server(
        dir,
        home,
        env,
        ca,
        gateway_uri,
        oauth_uri,
        "https://localhost:1",
        Some("must-never-be-used"),
    )
}

/// As [`config`], naming the config server and its token.
#[allow(clippy::too_many_arguments)]
pub fn config_with_server(
    dir: &TempDir,
    home: &str,
    env: &str,
    ca: &Path,
    gateway_uri: &str,
    oauth_uri: &str,
    config_server: &str,
    token: Option<&str>,
) -> CliConfig {
    let conf = dir.path().join(format!("conf-{home}"));
    std::fs::create_dir_all(&conf).unwrap();
    let authorization = token.map(|t| format!("Bearer {t}")).unwrap_or_default();
    std::fs::write(
        conf.join("startup.yml"),
        format!(
            "host: dev.lightapi.net\nserviceId: com.networknt.light-cli-1.0.0\nenvTag: {env}\n\
             configServerUri: {config_server}\nauthorization: \"{authorization}\"\n\
             bootstrapCaCertPath: {}\n",
            ca.display()
        ),
    )
    .unwrap();
    set_cli_yml(dir, home, gateway_uri, oauth_uri);
    CliConfig::load(&conf.join("startup.yml"), Some(dir.path().join(home)))
        .expect("startup.yml loads")
}

/// (Re)write `cli.yml`. An empty `oauth_uri` leaves the sign-in settings unset.
pub fn set_cli_yml(dir: &TempDir, home: &str, gateway_uri: &str, oauth_uri: &str) {
    let conf = dir.path().join(format!("conf-{home}"));
    std::fs::create_dir_all(&conf).unwrap();
    let (provider, client) = if oauth_uri.is_empty() {
        ("", "")
    } else {
        ("prov", "client-1")
    };
    std::fs::write(
        conf.join("cli.yml"),
        format!(
            "gatewayUri: ${{cli.gatewayUri:{gateway_uri}}}\n\
             agentServiceIds: ${{cli.agentServiceIds:com.networknt.agent.advisor-1.0.0,com.networknt.agent.tech-support-1.0.0}}\n\
             oauthUri: ${{cli.oauthUri:{oauth_uri}}}\noauthProviderId: ${{cli.oauthProviderId:{provider}}}\n\
             oauthClientId: ${{cli.oauthClientId:{client}}}\n"
        ),
    )
    .unwrap();
}

pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// A small HTTPS/1.1 server for stand-ins
// ---------------------------------------------------------------------------

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    /// The `application/x-www-form-urlencoded` body, if any.
    pub form: HashMap<String, String>,
    /// The raw body.
    pub body: Vec<u8>,
    /// Whether the client sent a certificate. The server never asks for one, so a well-behaved
    /// CLI never does: this is what a public client looks like on the wire.
    pub client_certificates: usize,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }
}

pub struct TlsServer {
    pub base: String,
    /// Every request that got past the TLS layer, in order.
    pub seen: Arc<Mutex<Vec<Request>>>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Serve HTTPS on a loopback port. Each request is recorded and answered by `handler`. No client
/// certificate is requested.
pub async fn start_tls_server(
    server_cert: &Path,
    server_key: &Path,
    handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
) -> TlsServer {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(server_cert)
        .unwrap()
        .map(|c| c.unwrap())
        .collect();
    let key = PrivateKeyDer::from_pem_file(server_key).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
    let log = Arc::clone(&seen);
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, log, handler) =
                (acceptor.clone(), Arc::clone(&log), Arc::clone(&handler));
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let client_certificates = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|c| c.len())
                    .unwrap_or(0);
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let Ok(n) = tls.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(at) = find(&buf, b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut words = head.split_whitespace();
                let method = words.next().unwrap_or_default().to_string();
                let path = words.next().unwrap_or_default().to_string();
                let headers: HashMap<String, String> = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + length {
                    let Ok(n) = tls.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = buf[header_end..header_end + length].to_vec();
                let form = url::form_urlencoded::parse(&body)
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                let request = Request {
                    method,
                    path,
                    headers,
                    form,
                    body,
                    client_certificates,
                };
                let reply = handler(&request);
                log.lock().unwrap().push(request);
                let extra: String = reply
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}\r\n"))
                    .collect();
                let response = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    TlsServer {
        base: format!("https://localhost:{port}"),
        seen,
    }
}

// ---------------------------------------------------------------------------
// A WebSocket server for stand-ins
// ---------------------------------------------------------------------------

/// What the client asked for in its WebSocket upgrade.
#[derive(Clone, Debug)]
pub struct Upgrade {
    /// Path and query.
    pub uri: String,
    /// Lower-cased header names.
    pub headers: HashMap<String, String>,
}

impl Upgrade {
    pub fn query(&self, name: &str) -> Option<String> {
        let query = self.uri.split_once('?')?.1;
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }
}

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

pub struct WsServer {
    /// `https://localhost:<port>`, the Gateway as the CLI is configured to see it.
    pub base: String,
    /// Every upgrade request that reached the server, in order.
    pub upgrades: Arc<Mutex<Vec<Upgrade>>>,
}

/// Serve WebSockets over TLS on a loopback port. `refuse` may answer an upgrade with an HTTP
/// status instead of accepting it; accepted connections go to `handler` with their index.
pub async fn start_ws_server(
    server_cert: &Path,
    server_key: &Path,
    refuse: impl Fn(&Upgrade, usize) -> Option<u16> + Send + Sync + 'static,
    handler: impl Fn(usize, Ws) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> WsServer {
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse, Request as WsRequest, Response as WsResponse,
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(server_cert)
        .unwrap()
        .map(|c| c.unwrap())
        .collect();
    let key = PrivateKeyDer::from_pem_file(server_key).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let upgrades: Arc<Mutex<Vec<Upgrade>>> = Arc::default();
    let log = Arc::clone(&upgrades);
    let (refuse, handler) = (Arc::new(refuse), Arc::new(handler));

    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, log, refuse, handler) = (
                acceptor.clone(),
                Arc::clone(&log),
                Arc::clone(&refuse),
                Arc::clone(&handler),
            );
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let index = std::sync::atomic::AtomicUsize::new(usize::MAX);
                #[allow(clippy::result_large_err)]
                let callback = |request: &WsRequest,
                                response: WsResponse|
                 -> Result<WsResponse, ErrorResponse> {
                    let upgrade = Upgrade {
                        uri: request.uri().to_string(),
                        headers: request
                            .headers()
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.as_str().to_ascii_lowercase(),
                                    v.to_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect(),
                    };
                    let mut seen = log.lock().unwrap();
                    let n = seen.len();
                    let verdict = refuse(&upgrade, n);
                    seen.push(upgrade);
                    index.store(n, std::sync::atomic::Ordering::SeqCst);
                    match verdict {
                        Some(status) => {
                            Err(tokio_tungstenite::tungstenite::http::Response::builder()
                                .status(status)
                                .body(Some("refused".to_string()))
                                .unwrap())
                        }
                        None => Ok(response),
                    }
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(tls, callback).await else {
                    return;
                };
                handler(index.load(std::sync::atomic::Ordering::SeqCst), ws).await;
            });
        }
    });
    WsServer {
        base: format!("https://localhost:{port}"),
        upgrades,
    }
}
