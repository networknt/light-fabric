use crate::protocol::{
    DiscoverySnapshot, DiscoverySubscription, ServiceMetadataUpdate, ServiceRegistrationParams,
};
use rustls::pki_types::CertificateDer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufReader, Cursor};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::{error, info};
use url::Url;
use uuid::Uuid;

use crate::logical::{PendingRequests, RuntimeResponse, RuntimeSessionOutput, handle_inbound};
use crate::websocket::{InboundEvent, WebSocketAdapter};

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
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<RuntimeSessionOutput>>>>,
    registration_tx: watch::Sender<RegistrationState>,
    ca_certificate: Mutex<Option<Vec<u8>>>,
    verify_hostname: Mutex<bool>,
    pending_requests: PendingRequests,
    heartbeat_interval: Duration,
}

#[derive(Clone)]
pub struct PortalRegistryNotifier {
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<RuntimeSessionOutput>>>>,
}

impl PortalRegistryNotifier {
    pub async fn send_notification(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        let payload = RuntimeSessionOutput::Notification {
            method: method.to_string(),
            params,
        };

        let tx = {
            let guard = self.outbound_tx.lock().await;
            guard.clone()
        };

        let tx = tx.ok_or_else(|| anyhow::anyhow!("registry client is not connected"))?;
        tx.send(payload)
            .await
            .map_err(|_| anyhow::anyhow!("registry client connection is closed"))
    }
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
            heartbeat_interval: Duration::from_secs(30),
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

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
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

    pub fn notifier(&self) -> PortalRegistryNotifier {
        PortalRegistryNotifier {
            outbound_tx: Arc::clone(&self.outbound_tx),
        }
    }

    pub async fn send_metadata_update(&self, update: ServiceMetadataUpdate) -> anyhow::Result<()> {
        let payload = RuntimeSessionOutput::Notification {
            method: "service/update_metadata".to_string(),
            params: serde_json::to_value(update)?,
        };

        let tx = {
            let guard = self.outbound_tx.lock().await;
            guard.clone()
        };

        let tx = tx.ok_or_else(|| anyhow::anyhow!("registry client is not connected"))?;
        tx.send(payload)
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
        let payload = RuntimeSessionOutput::Request {
            request_id: json!(id),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        let (tx, rx) = oneshot::channel::<RuntimeResponse>();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id.clone(), tx);
        }

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
        if outbound.send(payload).await.is_err() {
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
            let connection_start = std::time::Instant::now();
            let result = self.connect_and_loop().await;
            let connection_elapsed = connection_start.elapsed();

            let jitter_ms = rand::random::<u64>() % 1000;
            let total_delay = retry_delay + Duration::from_millis(jitter_ms);

            match result {
                Ok(_) => {
                    info!(
                        "Registry connection closed normally. Reconnecting in {:?}",
                        total_delay
                    );
                }
                Err(e) => {
                    error!(
                        "Registry connection error: {:?}. Retrying in {:?}",
                        e, total_delay
                    );
                }
            }

            tokio::time::sleep(total_delay).await;

            if connection_elapsed > Duration::from_secs(10) {
                retry_delay = Duration::from_secs(1);
            } else {
                retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(60));
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

        let mut transport = WebSocketAdapter::connect(&controller_url, connector).await?;
        info!("Connected to controller at {}", controller_url);

        self.register(&mut transport).await?;

        let (mut sender, mut receiver) = transport.split();
        let (tx, mut rx) = mpsc::channel::<RuntimeSessionOutput>(100);
        {
            let mut guard = self.outbound_tx.lock().await;
            *guard = Some(tx.clone());
        }

        let handler = self.handler.read().await.clone();
        let outbound_state = Arc::clone(&self.outbound_tx);

        let mut heartbeat = tokio::time::interval(self.heartbeat_interval);
        // Reset the heartbeat interval so it doesn't fire immediately
        heartbeat.tick().await;

        let mut ping_outstanding = false;

        let res = async {
            loop {
                tokio::select! {
                    res = receiver.next() => {
                        match res {
                            Ok(Some(event)) => {
                                ping_outstanding = false;
                                match event {
                                    InboundEvent::Message(message) => {
                                        if let Some(response) = handle_inbound(
                                            &handler,
                                            &self.pending_requests,
                                            message,
                                        ).await {
                                            tx.send(response).await.map_err(|_| {
                                                anyhow::anyhow!("registry client outbound queue closed")
                                            })?;
                                        }
                                    }
                                    InboundEvent::Ping(payload) => sender.send_pong(payload).await?,
                                    InboundEvent::Pong | InboundEvent::Ignored => {}
                                    InboundEvent::Close => break,
                                }
                            }
                            Ok(None) => break,
                            Err(error) => return Err(error),
                        }
                    }
                    Some(msg) = rx.recv() => {
                        sender.send_message(msg).await?;
                    }
                    _ = heartbeat.tick() => {
                        if ping_outstanding {
                            return Err(anyhow::anyhow!(
                                "heartbeat timeout: no pong received within {:?}",
                                self.heartbeat_interval
                            ));
                        }
                        ping_outstanding = true;
                        sender.send_ping().await?;
                    }
                }
            }
            Ok(())
        }
        .await;

        {
            let mut guard = outbound_state.lock().await;
            *guard = None;
        }
        self.pending_requests.lock().await.clear();
        let _ = self.registration_tx.send(RegistrationState::Disconnected);

        res
    }

    async fn register(&self, transport: &mut WebSocketAdapter) -> anyhow::Result<()> {
        let register_params = self.registration_params.lock().await.clone();
        let response = transport.register(register_params).await?;
        let _ = self.registration_tx.send(RegistrationState::Registered {
            runtime_instance_id: response.runtime_instance_id,
        });
        info!(
            "Successfully registered with controller. Instance ID: {}",
            response.runtime_instance_id
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::WebSocketAdapter;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

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

        let ws_stream = {
            let (ws_stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
                .await
                .expect("connect websocket");
            ws_stream
        };

        let mut transport = WebSocketAdapter::from_stream(ws_stream);
        client
            .register(&mut transport)
            .await
            .expect("register succeeds after ping");

        let state = registration_rx.borrow().clone();
        assert!(matches!(state, RegistrationState::Registered { .. }));
    }

    #[tokio::test]
    async fn test_registry_client_reconnects_and_reregisters_on_run_level() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let (register_tx1, register_rx1) = oneshot::channel();
        let (register_tx2, register_rx2) = oneshot::channel();

        tokio::spawn(async move {
            // First connection
            let (stream, _) = listener.accept().await.expect("accept first connection");
            let mut ws = accept_async(stream).await.expect("accept first websocket");
            let register1 = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let _ = register_tx1.send(serde_json::from_str::<Value>(&register1).unwrap());
            ws.send(Message::Text(json!({
                "jsonrpc": "2.0",
                "id": "register-1",
                "result": { "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39", "status": "registered" }
            }).to_string().into())).await.unwrap();
            ws.close(None).await.unwrap();

            // Second connection
            let (stream, _) = listener.accept().await.expect("accept second connection");
            let mut ws = accept_async(stream).await.expect("accept second websocket");
            let register2 = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let _ = register_tx2.send(serde_json::from_str::<Value>(&register2).unwrap());
            ws.send(Message::Text(json!({
                "jsonrpc": "2.0",
                "id": "register-1",
                "result": { "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40", "status": "registered" }
            }).to_string().into())).await.unwrap();
            ws.close(None).await.unwrap();
        });

        let client = Arc::new(
            PortalRegistryClient::new(
                &format!("ws://{addr}"),
                ServiceRegistrationParams {
                    service_id: "reconnect-service".to_string(),
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
            .unwrap(),
        );

        let task_client = Arc::clone(&client);
        let run_task = tokio::spawn(async move {
            task_client.run().await;
        });

        let register1 = tokio::time::timeout(Duration::from_secs(5), register_rx1)
            .await
            .expect("Timeout waiting for first registration handshake")
            .expect("register_rx1 closed");
        assert_eq!(register1["method"], "service/register");
        assert_eq!(register1["params"]["serviceId"], "reconnect-service");

        let register2 = tokio::time::timeout(Duration::from_secs(5), register_rx2)
            .await
            .expect("Timeout waiting for second registration handshake")
            .expect("register_rx2 closed");
        assert_eq!(register2["method"], "service/register");
        assert_eq!(register2["params"]["serviceId"], "reconnect-service");

        run_task.abort();
    }

    #[tokio::test]
    async fn test_heartbeat_timeout_detects_silent_controller_loss() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let (register_tx, register_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let mut ws = accept_async(stream).await.expect("accept websocket");
            let register = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let _ = register_tx.send(serde_json::from_str::<Value>(&register).unwrap());
            ws.send(Message::Text(json!({
                "jsonrpc": "2.0",
                "id": "register-1",
                "result": { "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39", "status": "registered" }
            }).to_string().into())).await.unwrap();

            // Keep the connection open indefinitely without reading from it,
            // so we do not automatically respond with a Pong to the client's Ping.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });

        let client = Arc::new(
            PortalRegistryClient::new(
                &format!("ws://{addr}"),
                ServiceRegistrationParams {
                    service_id: "heartbeat-service".to_string(),
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
            .unwrap()
            .with_heartbeat_interval(Duration::from_millis(100)),
        );

        let registration_rx = client.subscribe_registration();
        let task_client = Arc::clone(&client);
        let run_task = tokio::spawn(async move { task_client.connect_and_loop().await });

        // Wait for registration
        let register = tokio::time::timeout(Duration::from_secs(5), register_rx)
            .await
            .expect("register_rx timeout")
            .unwrap();
        assert_eq!(register["method"], "service/register");

        // Yield to allow the client to process the registration response and update its state
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify we are registered initially
        assert!(matches!(
            *registration_rx.borrow(),
            RegistrationState::Registered { .. }
        ));

        // The task should complete with Err (heartbeat timeout) within 500ms
        let res = tokio::time::timeout(Duration::from_millis(500), run_task)
            .await
            .expect("run_task join timeout")
            .unwrap();
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("heartbeat timeout"));

        // Verify state is Disconnected
        assert_eq!(*registration_rx.borrow(), RegistrationState::Disconnected);
    }
}
