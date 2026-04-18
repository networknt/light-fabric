use crate::protocol::{
    JsonRpcMessage, RegistrationResponse, ServiceMetadataUpdate, ServiceRegistrationParams,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, watch};
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
    roots: rustls::RootCertStore,
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
        let cert = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.supported_algs.all,
        )?;
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
    controller_url: Url,
    registration_params: ServiceRegistrationParams,
    handler: Arc<dyn RegistryHandler>,
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<Message>>>>,
    registration_tx: watch::Sender<RegistrationState>,
    ca_certificate: Option<Vec<u8>>,
    verify_hostname: bool,
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
            controller_url: url,
            registration_params,
            handler,
            outbound_tx: Arc::new(Mutex::new(None)),
            registration_tx,
            ca_certificate: None,
            verify_hostname: true,
        })
    }

    pub fn with_ca_certificate(mut self, ca_cert: Vec<u8>) -> Self {
        self.ca_certificate = Some(ca_cert);
        self
    }

    pub fn with_verify_hostname(mut self, verify: bool) -> Self {
        self.verify_hostname = verify;
        self
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
        let connector = if self.controller_url.scheme() == "wss"
            && (self.ca_certificate.is_some() || !self.verify_hostname)
        {
            if !self.verify_hostname && self.ca_certificate.is_none() {
                return Err(anyhow::anyhow!(
                    "verify_hostname=false requires an explicit CA certificate"
                ));
            }

            let mut root_store = rustls::RootCertStore::empty();
            if let Some(ca_cert) = &self.ca_certificate {
                let mut reader = std::io::BufReader::new(std::io::Cursor::new(ca_cert));
                for cert in rustls_pemfile::certs(&mut reader) {
                    root_store.add(cert?)?;
                }
            }

            let builder = rustls::ClientConfig::builder();

            if !self.verify_hostname {
                let supported_algs = rustls::crypto::CryptoProvider::get_default()
                    .map(|provider| provider.signature_verification_algorithms)
                    .unwrap_or_else(|| {
                        rustls::crypto::ring::default_provider().signature_verification_algorithms
                    });
                let verifier = Arc::new(NoHostnameVerifier {
                    roots: root_store,
                    supported_algs,
                });
                let config = builder
                    .dangerous()
                    .with_custom_certificate_verifier(verifier)
                    .with_no_client_auth();
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
            } else {
                let config = builder
                    .with_root_certificates(root_store)
                    .with_no_client_auth();
                Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
            }
        } else {
            None
        };

        let (mut ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            self.controller_url.as_str(),
            None,
            false,
            connector,
        )
        .await?;
        info!("Connected to controller at {}", self.controller_url);

        // 1. Initial Handshake (service/register)
        self.register(&mut ws_stream).await?;

        // 2. Main Loop
        let (mut sender, mut receiver) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<Message>(100);
        {
            let mut guard = self.outbound_tx.lock().await;
            *guard = Some(tx.clone());
        }

        let handler = Arc::clone(&self.handler);
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
                            if json_msg.id.is_some() {
                                // Request
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
                                // Notification
                                handler
                                    .handle_notification(
                                        method,
                                        json_msg.params.unwrap_or(json!({})),
                                    )
                                    .await;
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
        let _ = self.registration_tx.send(RegistrationState::Disconnected);

        Ok(())
    }

    async fn register(&self, ws_stream: &mut WsStream) -> anyhow::Result<()> {
        let registration_id = json!("register-1");
        let register_params_val = serde_json::to_value(&self.registration_params)?;
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

    struct NoopHandler;

    #[async_trait::async_trait]
    impl RegistryHandler for NoopHandler {}

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
