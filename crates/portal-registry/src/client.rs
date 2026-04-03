use crate::protocol::{
    JsonRpcMessage, RegistrationResponse, ServiceMetadataUpdate, ServiceRegistrationParams,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info};
use url::Url;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
}

impl PortalRegistryClient {
    pub fn new(
        controller_url: &str,
        registration_params: ServiceRegistrationParams,
        handler: Arc<dyn RegistryHandler>,
    ) -> anyhow::Result<Self> {
        let url = Url::parse(controller_url)?;
        Ok(Self {
            controller_url: url,
            registration_params,
            handler,
            outbound_tx: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn send_metadata_update(
        &self,
        update: ServiceMetadataUpdate,
    ) -> anyhow::Result<()> {
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
                    error!("Registry connection error: {:?}. Retrying in {:?}", e, retry_delay);
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(60));
                }
            }
        }
    }

    async fn connect_and_loop(&self) -> anyhow::Result<()> {
        let (mut ws_stream, _) = connect_async(self.controller_url.as_str()).await?;
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
                                let result = handler.handle_request(method, json_msg.params.unwrap_or(json!({}))).await;
                                let response = JsonRpcMessage {
                                    jsonrpc: "2.0".to_string(),
                                    id: json_msg.id,
                                    method: None,
                                    params: None,
                                    result: Some(result),
                                    error: None,
                                };
                                let _ = tx_clone.send(Message::Text(serde_json::to_string(&response)?.into())).await;
                            } else {
                                // Notification
                                handler.handle_notification(method, json_msg.params.unwrap_or(json!({}))).await;
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

        ws_stream.send(Message::Text(serde_json::to_string(&register_msg)?.into())).await?;

        if let Some(msg) = ws_stream.next().await {
            let text = msg?.into_text()?;
            let resp = serde_json::from_str::<JsonRpcMessage>(&text)?;
            if let Some(result) = resp.result {
                let reg_resp: RegistrationResponse = serde_json::from_value(result)?;
                info!("Successfully registered with controller. Instance ID: {}", reg_resp.runtime_instance_id);
                return Ok(());
            } else if let Some(error) = resp.error {
                return Err(anyhow::anyhow!("Registration failed: {}", error.message));
            }
        }

        Err(anyhow::anyhow!("Connection closed during registration"))
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
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
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
}
