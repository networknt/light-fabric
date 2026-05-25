use crate::protocol::{
    DiscoverySnapshot, DiscoverySubscription, JsonRpcMessage, RegistrationResponse,
    ServiceMetadataUpdate, ServiceRegistrationParams,
};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::CertificateDer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufReader, Cursor};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info};
use url::Url;
use uuid::Uuid;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
const REGISTRATION_ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct NoHostnameVerifier {
    roots: Arc<rustls::RootCertStore>,
    supported_algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for NoHostnameVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let cert = match rustls::server::ParsedCertificate::try_from(end_entity) {
            Ok(cert) => cert,
            Err(error) => {
                log_controller_certificate_verification_failure(
                    "parse peer certificate",
                    false,
                    end_entity,
                    intermediates,
                    &error,
                );
                return Err(error);
            }
        };

        if let Err(error) = rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.supported_algs.all,
        ) {
            log_controller_certificate_verification_failure(
                "verify peer certificate chain",
                false,
                end_entity,
                intermediates,
                &error,
            );
            return Err(error);
        }

        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

#[derive(Debug)]
struct LoggingHostnameVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for LoggingHostnameVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(error) => {
                log_controller_certificate_verification_failure(
                    "verify peer certificate",
                    true,
                    end_entity,
                    intermediates,
                    &error,
                );
                Err(error)
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CertificateIdentity {
    subject: String,
    issuer: String,
}

fn certificate_identity(certificate: &CertificateDer<'_>) -> CertificateIdentity {
    match x509_parser::parse_x509_certificate(certificate.as_ref()) {
        Ok((_remaining, parsed)) => CertificateIdentity {
            subject: parsed.subject().to_string(),
            issuer: parsed.issuer().to_string(),
        },
        Err(error) => CertificateIdentity {
            subject: format!("<unparsed: {error}>"),
            issuer: "<unparsed>".to_string(),
        },
    }
}

fn log_controller_certificate_verification_failure(
    verification_step: &str,
    verify_hostname: bool,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    error: &rustls::Error,
) {
    let peer = certificate_identity(end_entity);
    let intermediate_identities: Vec<CertificateIdentity> =
        intermediates.iter().map(certificate_identity).collect();
    let intermediate_subjects: Vec<&str> = intermediate_identities
        .iter()
        .map(|identity| identity.subject.as_str())
        .collect();
    let intermediate_issuers: Vec<&str> = intermediate_identities
        .iter()
        .map(|identity| identity.issuer.as_str())
        .collect();

    error!(
        verification_step,
        verify_hostname,
        peer_cert_subject = %peer.subject,
        peer_cert_issuer = %peer.issuer,
        intermediate_cert_count = intermediates.len(),
        intermediate_cert_subjects = ?intermediate_subjects,
        intermediate_cert_issuers = ?intermediate_issuers,
        error = %error,
        "portal-registry controller certificate verification failed"
    );
}

fn root_store_with_ca_bundle(
    ca_certificate: Option<&[u8]>,
) -> anyhow::Result<(rustls::RootCertStore, usize)> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let Some(ca_certificate) = ca_certificate else {
        return Ok((root_store, 0));
    };

    validate_ca_bundle_pem_labels(ca_certificate)?;

    let mut reader = BufReader::new(Cursor::new(ca_certificate));
    let mut certificate_count = 0usize;
    loop {
        let Some(item) = rustls_pemfile::read_one(&mut reader)? else {
            break;
        };

        match item {
            rustls_pemfile::Item::X509Certificate(cert) => {
                root_store.add(cert)?;
                certificate_count += 1;
            }
            other => {
                return Err(anyhow::anyhow!(
                    "portal-registry CA bundle contains unsupported PEM block: {:?}",
                    other
                ));
            }
        }
    }

    if certificate_count == 0 {
        return Err(anyhow::anyhow!(
            "portal-registry CA bundle contains no certificates"
        ));
    }

    Ok((root_store, certificate_count))
}

fn validate_ca_bundle_pem_labels(ca_certificate: &[u8]) -> anyhow::Result<()> {
    let text = std::str::from_utf8(ca_certificate)?;
    for line in text.lines().map(str::trim) {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|value| value.strip_suffix("-----"))
        else {
            continue;
        };
        if label != "CERTIFICATE" {
            return Err(anyhow::anyhow!(
                "portal-registry CA bundle contains unsupported PEM block: {label}"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationState {
    Disconnected,
    Registered { runtime_instance_id: Uuid },
}

#[async_trait::async_trait]
pub trait RegistryHandler: Send + Sync {
    async fn handle_notification(&self, _method: &str, _params: serde_json::Value) {}
    async fn handle_request(&self, _method: &str, _params: serde_json::Value) -> serde_json::Value {
        json!({"status": "received"})
    }
}

pub struct PortalRegistryClient {
    controller_url: Mutex<Url>,
    registration_params: Mutex<ServiceRegistrationParams>,
    handler: RwLock<Arc<dyn RegistryHandler>>,
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<Message>>>>,
    registration_tx: watch::Sender<RegistrationState>,
    ca_certificate: Mutex<Option<Vec<u8>>>,
    verify_hostname: Mutex<bool>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcMessage>>>>,
}

impl fmt::Debug for PortalRegistryClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalRegistryClient")
            .field(
                "connected",
                &self
                    .outbound_tx
                    .try_lock()
                    .ok()
                    .and_then(|tx| tx.as_ref().map(|_| true))
                    .unwrap_or(false),
            )
            .finish_non_exhaustive()
    }
}

impl PortalRegistryClient {
    pub fn new(
        controller_url: &str,
        registration_params: ServiceRegistrationParams,
        handler: Arc<dyn RegistryHandler>,
    ) -> anyhow::Result<Self> {
        let url = Url::parse(controller_url)?;
        let (registration_tx, _) = watch::channel(RegistrationState::Disconnected);
        Ok(Self {
            controller_url: Mutex::new(url),
            registration_params: Mutex::new(registration_params),
            handler: RwLock::new(handler),
            outbound_tx: Arc::new(Mutex::new(None)),
            registration_tx,
            ca_certificate: Mutex::new(None),
            verify_hostname: Mutex::new(true),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn with_ca_certificate(mut self, ca_cert: Vec<u8>) -> Self {
        self.ca_certificate = Mutex::new(Some(ca_cert));
        self
    }

    pub fn with_verify_hostname(mut self, verify: bool) -> Self {
        self.verify_hostname = Mutex::new(verify);
        self
    }

    pub async fn set_registration_params(&self, registration_params: ServiceRegistrationParams) {
        let mut guard = self.registration_params.lock().await;
        *guard = registration_params;
    }

    pub async fn configure_connection(
        &self,
        controller_url: &str,
        ca_certificate: Option<Vec<u8>>,
        verify_hostname: bool,
    ) -> anyhow::Result<()> {
        let url = Url::parse(controller_url)?;
        {
            let mut guard = self.controller_url.lock().await;
            *guard = url;
        }
        {
            let mut guard = self.ca_certificate.lock().await;
            *guard = ca_certificate;
        }
        {
            let mut guard = self.verify_hostname.lock().await;
            *guard = verify_hostname;
        }
        Ok(())
    }

    pub async fn set_handler(&self, handler: Arc<dyn RegistryHandler>) {
        let mut guard = self.handler.write().await;
        *guard = handler;
    }

    pub fn subscribe_registration(&self) -> watch::Receiver<RegistrationState> {
        self.registration_tx.subscribe()
    }

    pub async fn send_metadata_update(&self, update: ServiceMetadataUpdate) -> anyhow::Result<()> {
        let payload = JsonRpcMessage::new_notification(
            "service/update_metadata",
            serde_json::to_value(update)?,
        );
        let message = Message::Text(serde_json::to_string(&payload)?.into());

        let tx = {
            let guard = self.outbound_tx.lock().await;
            guard.clone()
        };

        let tx = tx.ok_or_else(|| anyhow::anyhow!("registry client is not connected"))?;
        tx.send(message)
            .await
            .map_err(|_| anyhow::anyhow!("registry client connection is closed"))
    }

    pub async fn lookup_discovery(
        &self,
        subscription: DiscoverySubscription,
    ) -> anyhow::Result<DiscoverySnapshot> {
        self.send_request("discovery/lookup", subscription, Duration::from_secs(5))
            .await
    }

    async fn send_request<P, R>(
        &self,
        method: &str,
        params: P,
        request_timeout: Duration,
    ) -> anyhow::Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = Uuid::new_v4().to_string();
        let payload = JsonRpcMessage::new_request(json!(id), method, serde_json::to_value(params)?);

        let (tx, rx) = oneshot::channel::<JsonRpcMessage>();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id.clone(), tx);
        }

        let message = Message::Text(serde_json::to_string(&payload)?.into());
        let outbound = {
            let guard = self.outbound_tx.lock().await;
            guard.clone()
        };

        let outbound = match outbound {
            Some(outbound) => outbound,
            None => {
                self.pending_requests.lock().await.remove(&id);
                return Err(anyhow::anyhow!("registry client is not connected"));
            }
        };
        if outbound.send(message).await.is_err() {
            self.pending_requests.lock().await.remove(&id);
            return Err(anyhow::anyhow!("failed to send {method} request"));
        }

        let response = match timeout(request_timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.pending_requests.lock().await.remove(&id);
                return Err(anyhow::anyhow!("{method} response channel closed"));
            }
            Err(_) => {
                self.pending_requests.lock().await.remove(&id);
                return Err(anyhow::anyhow!("{method} timed out"));
            }
        };

        if let Some(error) = response.error {
            return Err(anyhow::anyhow!("{method} failed: {}", error.message));
        }

        let result = response
            .result
            .ok_or_else(|| anyhow::anyhow!("no result in {method} response"))?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn run(&self) {
        let mut retry_delay = Duration::from_secs(1);
        loop {
            match self.connect_and_loop().await {
                Ok(_) => {
                    info!("Registry connection closed normally, reconnecting...");
                    retry_delay = Duration::from_secs(1);
                }
                Err(e) => {
                    error!(
                        "Registry connection error: {:?}. Retrying in {:?}",
                        e, retry_delay
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(60));
                }
            }
        }
    }

    async fn connect_and_loop(&self) -> anyhow::Result<()> {
        let controller_url = self.controller_url.lock().await.clone();
        let ca_certificate = self.ca_certificate.lock().await.clone();
        let verify_hostname = *self.verify_hostname.lock().await;

        let connector = if controller_url.scheme() == "wss"
            && (ca_certificate.is_some() || !verify_hostname)
        {
            if !verify_hostname && ca_certificate.is_none() {
                return Err(anyhow::anyhow!(
                    "verify_hostname=false requires an explicit CA certificate for portal-registry; set startup.bootstrapCaCertPath or client.caCertPath"
                ));
            }

            let (root_store, ca_cert_count) = root_store_with_ca_bundle(ca_certificate.as_deref())?;
            info!(
                controller_url = %controller_url,
                ca_cert_count,
                verify_hostname,
                "loaded portal-registry CA certificate bundle"
            );

            let builder = rustls::ClientConfig::builder();
            let root_store = Arc::new(root_store);

            if !verify_hostname {
                let supported_algs = rustls::crypto::CryptoProvider::get_default()
                    .map(|provider| provider.signature_verification_algorithms)
                    .unwrap_or_else(|| {
                        rustls::crypto::ring::default_provider().signature_verification_algorithms
                    });
                let verifier = Arc::new(NoHostnameVerifier {
                    roots: Arc::clone(&root_store),
                    supported_algs,
                });
                let config = builder
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
                    .with_no_client_auth();
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
            } else {
                let inner = rustls::client::WebPkiServerVerifier::builder(root_store).build()?;
                let verifier = Arc::new(LoggingHostnameVerifier { inner });
                let config = builder
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
                    .with_no_client_auth();
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
            }
        } else {
            None
        };

        let (mut ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            controller_url.as_str(),
            None,
            false,
            connector,
        )
        .await?;
        info!("Connected to controller at {}", controller_url);

        // 1. Initial Handshake (service/register)
        self.register(&mut ws_stream).await?;

        // 2. Main Loop
        let (mut sender, mut receiver) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<Message>(100);
        {
            let mut guard = self.outbound_tx.lock().await;
            *guard = Some(tx.clone());
        }

        let handler = self.handler.read().await.clone();
        let outbound_state = Arc::clone(&self.outbound_tx);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = sender.send(msg).await {
                    error!("Failed to send websocket message: {:?}", e);
                    break;
                }
            }
        });

        while let Some(msg) = receiver.next().await {
            match msg? {
                Message::Text(text) => {
                    if let Ok(json_msg) = serde_json::from_str::<JsonRpcMessage>(&text) {
                        if let Some(method) = json_msg.method.as_deref() {
                            // Request or Notification
                            if json_msg.id.is_some() {
                                // Request from server
                                let result = handler
                                    .handle_request(method, json_msg.params.unwrap_or(json!({})))
                                    .await;
                                let response = JsonRpcMessage {
                                    jsonrpc: "2.0".to_string(),
                                    id: json_msg.id,
                                    method: None,
                                    params: None,
                                    result: Some(result),
                                    error: None,
                                };
                                let _ = tx_clone
                                    .send(Message::Text(serde_json::to_string(&response)?.into()))
                                    .await;
                            } else {
                                // Notification from server
                                handler
                                    .handle_notification(
                                        method,
                                        json_msg.params.unwrap_or(json!({})),
                                    )
                                    .await;
                            }
                        } else if let Some(id_val) = json_msg.id.as_ref() {
                            // Response to our request
                            let id_str = match id_val {
                                serde_json::Value::String(s) => s.clone(),
                                _ => id_val.to_string(),
                            };
                            let mut pending = self.pending_requests.lock().await;
                            if let Some(tx) = pending.remove(&id_str) {
                                let _ = tx.send(json_msg);
                            }
                        }
                    }
                }
                Message::Ping(payload) => {
                    let _ = tx_clone.send(Message::Pong(payload)).await;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }

        {
            let mut guard = outbound_state.lock().await;
            *guard = None;
        }
        self.pending_requests.lock().await.clear();
        let _ = self.registration_tx.send(RegistrationState::Disconnected);

        Ok(())
    }

    async fn register(&self, ws_stream: &mut WsStream) -> anyhow::Result<()> {
        let registration_id = json!("register-1");
        let register_params = self.registration_params.lock().await.clone();
        let register_params_val = serde_json::to_value(register_params)?;
        let register_msg = JsonRpcMessage::new_request(
            registration_id.clone(),
            "service/register",
            register_params_val,
        );

        ws_stream
            .send(Message::Text(serde_json::to_string(&register_msg)?.into()))
            .await?;

        timeout(REGISTRATION_ACK_TIMEOUT, async {
            while let Some(msg) = ws_stream.next().await {
                match msg? {
                    Message::Text(text) => {
                        debug!("Raw registration response: '{}'", text);
                        let resp = serde_json::from_str::<JsonRpcMessage>(&text)?;
                        if let Some(result) = resp.result {
                            let reg_resp: RegistrationResponse = serde_json::from_value(result)?;
                            let _ = self.registration_tx.send(RegistrationState::Registered {
                                runtime_instance_id: reg_resp.runtime_instance_id,
                            });
                            info!(
                                "Successfully registered with controller. Instance ID: {}",
                                reg_resp.runtime_instance_id
                            );
                            return Ok(());
                        } else if let Some(error) = resp.error {
                            return Err(anyhow::anyhow!("Registration failed: {}", error.message));
                        }
                    }
                    Message::Ping(payload) => {
                        ws_stream.send(Message::Pong(payload)).await?;
                    }
                    Message::Pong(_) => {}
                    Message::Close(Some(frame)) => {
                        return Err(anyhow::anyhow!(
                            "Connection closed during registration: code={} reason={}",
                            frame.code,
                            frame.reason
                        ));
                    }
                    Message::Close(None) => {
                        return Err(anyhow::anyhow!("Connection closed during registration"));
                    }
                    Message::Binary(_) => {
                        return Err(anyhow::anyhow!(
                            "Unexpected binary frame received during registration"
                        ));
                    }
                    Message::Frame(_) => {}
                }
            }

            Err(anyhow::anyhow!("Connection closed during registration"))
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Timed out waiting {:?} for controller registration acknowledgement",
                REGISTRATION_ACK_TIMEOUT
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_async;

    const TEST_CA_PEM: &[u8] = include_bytes!("../../../apps/light-gateway/config/ca.pem");

    struct NoopHandler;

    #[async_trait::async_trait]
    impl RegistryHandler for NoopHandler {}

    #[test]
    fn root_store_accepts_single_ca_certificate() {
        let (_root_store, certificate_count) =
            root_store_with_ca_bundle(Some(TEST_CA_PEM)).expect("root store");

        assert_eq!(certificate_count, 1);
    }

    #[test]
    fn certificate_identity_extracts_subject_and_issuer() {
        let mut reader = BufReader::new(Cursor::new(TEST_CA_PEM));
        let certificate = match rustls_pemfile::read_one(&mut reader)
            .expect("read test certificate")
            .expect("test certificate")
        {
            rustls_pemfile::Item::X509Certificate(certificate) => certificate,
            other => panic!("unexpected PEM item: {other:?}"),
        };

        let identity = certificate_identity(&certificate);

        assert!(identity.subject.contains("networknt-local-ca"));
        assert!(identity.issuer.contains("networknt-local-ca"));
    }

    #[test]
    fn root_store_rejects_empty_ca_bundle() {
        let error = root_store_with_ca_bundle(Some(b"# comment only\n"))
            .expect_err("empty CA bundle should fail")
            .to_string();

        assert!(error.contains("contains no certificates"));
    }

    #[test]
    fn root_store_rejects_non_certificate_pem_blocks() {
        let mut bundle = Vec::from(TEST_CA_PEM);
        bundle.extend_from_slice(
            b"-----BEGIN PRIVATE KEY-----\nnot-a-valid-key\n-----END PRIVATE KEY-----\n",
        );

        let error = root_store_with_ca_bundle(Some(&bundle))
            .expect_err("private key in CA bundle should fail")
            .to_string();

        assert!(error.contains("unsupported PEM block"));
    }

    #[tokio::test]
    async fn registration_and_metadata_update_match_controller_protocol() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let (register_tx, register_rx) = oneshot::channel();
        let (update_tx, update_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut ws = accept_async(stream).await.expect("accept websocket");

            let register = ws
                .next()
                .await
                .expect("register message")
                .expect("valid register frame")
                .into_text()
                .expect("register text");
            register_tx
                .send(serde_json::from_str::<Value>(&register).expect("register json"))
                .expect("send register payload");

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "register-1",
                    "result": {
                        "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39",
                        "status": "registered"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send ack");

            let update = ws
                .next()
                .await
                .expect("metadata message")
                .expect("valid metadata frame")
                .into_text()
                .expect("metadata text");
            update_tx
                .send(serde_json::from_str::<Value>(&update).expect("metadata json"))
                .expect("send metadata payload");
            ws.close(None).await.expect("close websocket");
        });

        let mut tags = HashMap::new();
        tags.insert("region".to_string(), "ca-central-1".to_string());

        let client = PortalRegistryClient::new(
            &format!("ws://{addr}"),
            ServiceRegistrationParams {
                service_id: "user-service".to_string(),
                version: "1.0.0".to_string(),
                protocol: "https".to_string(),
                address: "127.0.0.1".to_string(),
                port: 8443,
                tags,
                env_tag: Some("prod".to_string()),
                jwt: "token".to_string(),
            },
            Arc::new(NoopHandler),
        )
        .expect("build client");

        let client_task = tokio::spawn({
            let client = Arc::new(client);
            let task_client = Arc::clone(&client);
            async move {
                let run = tokio::spawn(async move { task_client.connect_and_loop().await });
                tokio::time::sleep(Duration::from_millis(50)).await;
                client
                    .send_metadata_update(ServiceMetadataUpdate {
                        protocol: Some("http".to_string()),
                        ..Default::default()
                    })
                    .await
                    .expect("send metadata update");
                run.await.expect("join client task").expect("client loop")
            }
        });

        let register = register_rx.await.expect("receive register");
        assert_eq!(register["method"], "service/register");
        assert_eq!(register["params"]["serviceId"], "user-service");
        assert_eq!(register["params"]["envTag"], "prod");
        assert_eq!(register["params"]["jwt"], "token");
        assert_eq!(register["params"]["tags"]["region"], "ca-central-1");

        let update = update_rx.await.expect("receive update");
        assert_eq!(update["method"], "service/update_metadata");
        assert_eq!(update["params"]["protocol"], "http");

        client_task.await.expect("join wrapper task");
    }

    #[tokio::test]
    async fn discovery_lookup_uses_registered_microservice_socket() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let (lookup_tx, lookup_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut ws = accept_async(stream).await.expect("accept websocket");

            let register = ws
                .next()
                .await
                .expect("register message")
                .expect("valid register frame")
                .into_text()
                .expect("register text");
            let register_json = serde_json::from_str::<Value>(&register).expect("register json");
            assert_eq!(register_json["method"], "service/register");

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "register-1",
                    "result": {
                        "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39",
                        "status": "registered"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send ack");

            let lookup = ws
                .next()
                .await
                .expect("lookup message")
                .expect("valid lookup frame")
                .into_text()
                .expect("lookup text");
            let lookup_json = serde_json::from_str::<Value>(&lookup).expect("lookup json");
            lookup_tx
                .send(lookup_json.clone())
                .expect("send lookup payload");

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": lookup_json["id"],
                    "result": {
                        "serviceId": "user-service",
                        "envTag": "prod",
                        "protocol": "https",
                        "nodes": [{
                            "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40",
                            "serviceId": "user-service",
                            "envTag": "prod",
                            "environment": "prod",
                            "version": "1.0.0",
                            "protocol": "https",
                            "address": "10.20.30.40",
                            "port": 8443,
                            "tags": { "zone": "a" },
                            "connectedAt": "2026-01-01T00:00:00Z",
                            "lastSeenAt": "2026-01-01T00:00:01Z",
                            "connected": true
                        }]
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send lookup response");
            ws.close(None).await.expect("close websocket");
        });

        let client = Arc::new(
            PortalRegistryClient::new(
                &format!("ws://{addr}"),
                ServiceRegistrationParams {
                    service_id: "subscriber-service".to_string(),
                    version: "1.0.0".to_string(),
                    protocol: "https".to_string(),
                    address: "127.0.0.1".to_string(),
                    port: 8443,
                    tags: HashMap::new(),
                    env_tag: Some("prod".to_string()),
                    jwt: "token".to_string(),
                },
                Arc::new(NoopHandler),
            )
            .expect("build client"),
        );
        let mut registration_rx = client.subscribe_registration();
        let task_client = Arc::clone(&client);
        let run = tokio::spawn(async move { task_client.connect_and_loop().await });
        while !matches!(
            registration_rx.borrow().clone(),
            RegistrationState::Registered { .. }
        ) {
            registration_rx
                .changed()
                .await
                .expect("registration change");
        }

        let snapshot = client
            .lookup_discovery(DiscoverySubscription {
                service_id: "user-service".to_string(),
                env_tag: Some("prod".to_string()),
                protocol: Some("https".to_string()),
            })
            .await
            .expect("lookup discovery");

        let lookup = lookup_rx.await.expect("receive lookup");
        assert_eq!(lookup["method"], "discovery/lookup");
        assert_eq!(lookup["params"]["serviceId"], "user-service");
        assert_eq!(lookup["params"]["envTag"], "prod");
        assert_eq!(lookup["params"]["protocol"], "https");
        assert_eq!(snapshot.nodes[0].address, "10.20.30.40");
        assert_eq!(snapshot.nodes[0].port, 8443);

        run.await.expect("join client task").expect("client loop");
    }

    #[tokio::test]
    async fn registration_succeeds_when_ping_arrives_before_ack() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut ws = accept_async(stream).await.expect("accept websocket");

            let register = ws
                .next()
                .await
                .expect("register message")
                .expect("valid register frame")
                .into_text()
                .expect("register text");
            let register_json = serde_json::from_str::<Value>(&register).expect("register json");
            assert_eq!(register_json["method"], "service/register");

            ws.send(Message::Ping("pre-ack".as_bytes().to_vec().into()))
                .await
                .expect("send ping");

            let pong = ws
                .next()
                .await
                .expect("pong frame")
                .expect("valid pong frame");
            match pong {
                Message::Pong(payload) => assert_eq!(payload.to_vec(), b"pre-ack"),
                other => panic!("expected pong, got {other:?}"),
            }

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "register-1",
                    "result": {
                        "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39",
                        "status": "registered"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send ack");

            ws.close(None).await.expect("close websocket");
        });

        let client = PortalRegistryClient::new(
            &format!("ws://{addr}"),
            ServiceRegistrationParams {
                service_id: "user-service".to_string(),
                version: "1.0.0".to_string(),
                protocol: "https".to_string(),
                address: "127.0.0.1".to_string(),
                port: 8443,
                tags: HashMap::new(),
                env_tag: None,
                jwt: "token".to_string(),
            },
            Arc::new(NoopHandler),
        )
        .expect("build client");
        let registration_rx = client.subscribe_registration();

        let mut ws_stream = {
            let (ws_stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
                .await
                .expect("connect websocket");
            ws_stream
        };

        client
            .register(&mut ws_stream)
            .await
            .expect("register succeeds after ping");

        let state = registration_rx.borrow().clone();
        assert!(matches!(state, RegistrationState::Registered { .. }));
    }
}
