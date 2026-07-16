use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::debug;
use url::Url;

use crate::candidate::{ConnectionFailure, ConnectionFailureClass, ControlCandidate};
use crate::logical::{
    RuntimeSessionInput, RuntimeSessionOutput, input_from_legacy_json, output_to_legacy_json,
};
use crate::protocol::{JsonRpcMessage, RegistrationResponse, ServiceRegistrationParams};

pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
const REGISTRATION_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum InboundEvent {
    Message(RuntimeSessionInput),
    ApplicationPing { nonce: u64, timestamp_ms: i64 },
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
        candidate: ControlCandidate,
        service_jwt: Option<&str>,
    ) -> Result<Self, ConnectionFailure> {
        let stream = connect_stream(controller_url, connector, candidate, service_jwt).await?;
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

pub(crate) async fn connect_stream(
    controller_url: &Url,
    connector: Option<tokio_tungstenite::Connector>,
    candidate: ControlCandidate,
    service_jwt: Option<&str>,
) -> Result<WsStream, ConnectionFailure> {
    let mut request = controller_url
        .as_str()
        .into_client_request()
        .map_err(classify_connect_error)?;
    if let Some(profile) = candidate.profile_token() {
        let jwt = service_jwt.filter(|jwt| !jwt.is_empty()).ok_or_else(|| {
            ConnectionFailure::new(
                ConnectionFailureClass::Authentication,
                "explicit runtime profile requires a service JWT",
            )
        })?;
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(profile));
        let mut authorization = HeaderValue::from_str(&format!("Bearer {jwt}")).map_err(|_| {
            ConnectionFailure::new(
                ConnectionFailureClass::Authentication,
                "service JWT cannot be represented as an Authorization header",
            )
        })?;
        authorization.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, authorization);
    }

    let (stream, response) = timeout(
        WEBSOCKET_HANDSHAKE_TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector),
    )
    .await
    .map_err(|_| {
        ConnectionFailure::new(
            ConnectionFailureClass::Unavailable,
            "WebSocket handshake timed out",
        )
    })?
    .map_err(classify_connect_error)?;
    let selected = response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());
    match candidate.profile_token() {
        Some(expected) if selected == Some(expected) => {}
        Some(_) => {
            return Err(ConnectionFailure::new(
                ConnectionFailureClass::Unsupported,
                "controller did not select the explicitly requested runtime profile",
            ));
        }
        None if selected.is_none() => {}
        None => {
            return Err(ConnectionFailure::new(
                ConnectionFailureClass::MalformedProfile,
                "controller selected a subprotocol for a legacy connection",
            ));
        }
    }
    Ok(stream)
}

fn classify_connect_error(error: tokio_tungstenite::tungstenite::Error) -> ConnectionFailure {
    use tokio_tungstenite::tungstenite::Error;

    let class = match &error {
        Error::Io(_) => ConnectionFailureClass::Unavailable,
        Error::Tls(_) => ConnectionFailureClass::Authentication,
        Error::Http(response) => match response.status().as_u16() {
            401 => ConnectionFailureClass::Authentication,
            403 => ConnectionFailureClass::Authorization,
            404 | 426 => ConnectionFailureClass::Unsupported,
            400 => ConnectionFailureClass::MalformedProfile,
            502..=504 => ConnectionFailureClass::Unavailable,
            _ => ConnectionFailureClass::Internal,
        },
        Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::SecWebSocketSubProtocolError(
                tokio_tungstenite::tungstenite::error::SubProtocolError::NoSubProtocol,
            ),
        ) => ConnectionFailureClass::Unsupported,
        Error::Url(_) | Error::Protocol(_) | Error::HttpFormat(_) => {
            ConnectionFailureClass::MalformedProfile
        }
        _ => ConnectionFailureClass::Internal,
    };
    ConnectionFailure::new(class, "WebSocket connection attempt failed")
}

pub(crate) struct WebSocketWriter {
    writer: SplitSink<WsStream, Message>,
}

impl WebSocketWriter {
    pub(crate) async fn send_message(
        &mut self,
        message: RuntimeSessionOutput,
    ) -> anyhow::Result<()> {
        let message = output_to_legacy_json(message);
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
                Ok(message) => InboundEvent::Message(input_from_legacy_json(message)),
                Err(_) => InboundEvent::Ignored,
            },
            Message::Ping(payload) => InboundEvent::Ping(payload.to_vec()),
            Message::Pong(_) => InboundEvent::Pong,
            Message::Close(_) => InboundEvent::Close,
            Message::Binary(_) | Message::Frame(_) => InboundEvent::Ignored,
        }))
    }
}
