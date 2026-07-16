use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::debug;
use url::Url;

use crate::protocol::{JsonRpcMessage, RegistrationResponse, ServiceRegistrationParams};

pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
const REGISTRATION_ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum InboundEvent {
    Message(JsonRpcMessage),
    Ping(Vec<u8>),
    Pong,
    Close,
    Ignored,
}

pub(crate) struct WebSocketAdapter {
    stream: WsStream,
}

impl WebSocketAdapter {
    pub(crate) async fn connect(
        controller_url: &Url,
        connector: Option<tokio_tungstenite::Connector>,
    ) -> anyhow::Result<Self> {
        let (stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            controller_url.as_str(),
            None,
            false,
            connector,
        )
        .await?;
        Ok(Self { stream })
    }

    #[cfg(test)]
    pub(crate) fn from_stream(stream: WsStream) -> Self {
        Self { stream }
    }

    pub(crate) async fn register(
        &mut self,
        params: ServiceRegistrationParams,
    ) -> anyhow::Result<RegistrationResponse> {
        let message = JsonRpcMessage::new_request(
            json!("register-1"),
            "service/register",
            serde_json::to_value(params)?,
        );
        self.stream
            .send(Message::Text(serde_json::to_string(&message)?))
            .await?;

        timeout(REGISTRATION_ACK_TIMEOUT, async {
            while let Some(frame) = self.stream.next().await {
                match frame? {
                    Message::Text(text) => {
                        debug!("Raw registration response: '{}'", text);
                        let response = serde_json::from_str::<JsonRpcMessage>(&text)?;
                        if let Some(result) = response.result {
                            return Ok(serde_json::from_value(result)?);
                        }
                        if let Some(error) = response.error {
                            return Err(anyhow::anyhow!("Registration failed: {}", error.message));
                        }
                    }
                    Message::Ping(payload) => self.stream.send(Message::Pong(payload)).await?,
                    Message::Pong(_) | Message::Frame(_) => {}
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

    pub(crate) fn split(self) -> (WebSocketWriter, WebSocketReader) {
        let (writer, reader) = self.stream.split();
        (WebSocketWriter { writer }, WebSocketReader { reader })
    }
}

pub(crate) struct WebSocketWriter {
    writer: SplitSink<WsStream, Message>,
}

impl WebSocketWriter {
    pub(crate) async fn send_message(&mut self, message: JsonRpcMessage) -> anyhow::Result<()> {
        self.writer
            .send(Message::Text(serde_json::to_string(&message)?))
            .await?;
        Ok(())
    }

    pub(crate) async fn send_ping(&mut self) -> anyhow::Result<()> {
        self.writer.send(Message::Ping(Vec::new())).await?;
        Ok(())
    }

    pub(crate) async fn send_pong(&mut self, payload: Vec<u8>) -> anyhow::Result<()> {
        self.writer.send(Message::Pong(payload)).await?;
        Ok(())
    }
}

pub(crate) struct WebSocketReader {
    reader: SplitStream<WsStream>,
}

impl WebSocketReader {
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<InboundEvent>> {
        let Some(frame) = self.reader.next().await else {
            return Ok(None);
        };
        Ok(Some(match frame? {
            Message::Text(text) => match serde_json::from_str(&text) {
                Ok(message) => InboundEvent::Message(message),
                Err(_) => InboundEvent::Ignored,
            },
            Message::Ping(payload) => InboundEvent::Ping(payload.to_vec()),
            Message::Pong(_) => InboundEvent::Pong,
            Message::Close(_) => InboundEvent::Close,
            Message::Binary(_) | Message::Frame(_) => InboundEvent::Ignored,
        }))
    }
}
