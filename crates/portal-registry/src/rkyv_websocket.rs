use std::time::Duration;

use controller_wire::v1::{ClientHelloV1, PongV1};
use controller_wire::{
    DecodedMessageV1, FrameHeaderV1, LogicalChannel, MessageKindV1,
    decode_rkyv_frame_v1_on_channel, encode_rkyv_frame_v1,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::candidate::{ConnectionFailure, ControlCandidate};
use crate::logical::RuntimeSessionOutput;
use crate::protocol::{RegistrationResponse, ServiceRegistrationParams};
use crate::websocket::{InboundEvent, WsStream, connect_stream};
use crate::wire::{input_from_wire, output_to_wire};

const REGISTRATION_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

pub(crate) struct RkyvWebSocketAdapter {
    stream: WsStream,
    max_payload_bytes: usize,
}

impl RkyvWebSocketAdapter {
    pub(crate) async fn connect(
        controller_url: &Url,
        connector: Option<tokio_tungstenite::Connector>,
        service_jwt: &str,
    ) -> Result<Self, ConnectionFailure> {
        let stream = connect_stream(
            controller_url,
            connector,
            ControlCandidate::runtime_rkyv_v1(),
            Some(service_jwt),
        )
        .await?;
        Ok(Self {
            stream,
            max_payload_bytes: INITIAL_MAX_PAYLOAD_BYTES,
        })
    }

    pub(crate) async fn register(
        &mut self,
        params: ServiceRegistrationParams,
    ) -> anyhow::Result<RegistrationResponse> {
        let hello = DecodedMessageV1::ClientHello(ClientHelloV1::from(&params));
        let frame = encode_rkyv_frame_v1(&hello, self.max_payload_bytes)?;
        self.stream.send(Message::Binary(frame)).await?;

        let server_hello = timeout(REGISTRATION_ACK_TIMEOUT, async {
            while let Some(frame) = self.stream.next().await {
                match frame? {
                    Message::Binary(frame) => {
                        let message = decode_rkyv_frame_v1_on_channel(
                            &frame,
                            self.max_payload_bytes,
                            LogicalChannel::SessionControl,
                        )?;
                        return match message {
                            DecodedMessageV1::ServerHello(hello) => Ok(hello),
                            _ => Err(anyhow::anyhow!(
                                "expected server_hello as registration acknowledgement"
                            )),
                        };
                    }
                    Message::Ping(payload) => self.stream.send(Message::Pong(payload)).await?,
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Text(_) => {
                        return Err(anyhow::anyhow!(
                            "unexpected text frame during rkyv registration"
                        ));
                    }
                    Message::Close(_) => {
                        return Err(anyhow::anyhow!(
                            "connection closed during rkyv registration"
                        ));
                    }
                }
            }
            Err(anyhow::anyhow!(
                "connection closed during rkyv registration"
            ))
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for server_hello"))??;

        self.max_payload_bytes = usize::try_from(server_hello.max_control_payload_bytes)
            .unwrap_or(INITIAL_MAX_PAYLOAD_BYTES);
        Ok(RegistrationResponse {
            runtime_instance_id: server_hello.runtime_instance_id,
            status: "registered".to_string(),
        })
    }

    pub(crate) fn split(self) -> (RkyvWebSocketWriter, RkyvWebSocketReader) {
        let (writer, reader) = self.stream.split();
        (
            RkyvWebSocketWriter {
                writer,
                max_payload_bytes: self.max_payload_bytes,
                notification_sequence: 1,
            },
            RkyvWebSocketReader {
                reader,
                max_payload_bytes: self.max_payload_bytes,
            },
        )
    }
}

pub(crate) struct RkyvWebSocketWriter {
    writer: SplitSink<WsStream, Message>,
    max_payload_bytes: usize,
    notification_sequence: u64,
}

impl RkyvWebSocketWriter {
    pub(crate) async fn send_message(
        &mut self,
        message: RuntimeSessionOutput,
    ) -> anyhow::Result<()> {
        let mut message = output_to_wire(message)?;
        if let DecodedMessageV1::RuntimeNotification(notification) = &mut message {
            notification.sequence = self.notification_sequence;
            self.notification_sequence = self.notification_sequence.saturating_add(1);
        }
        self.send_wire(message).await
    }

    pub(crate) async fn send_application_pong(
        &mut self,
        nonce: u64,
        timestamp_ms: i64,
    ) -> anyhow::Result<()> {
        self.send_wire(DecodedMessageV1::Pong(PongV1 {
            nonce,
            timestamp_ms,
        }))
        .await
    }

    async fn send_wire(&mut self, message: DecodedMessageV1) -> anyhow::Result<()> {
        let frame = encode_rkyv_frame_v1(&message, self.max_payload_bytes)?;
        self.writer.send(Message::Binary(frame)).await?;
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

pub(crate) struct RkyvWebSocketReader {
    reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    max_payload_bytes: usize,
}

impl RkyvWebSocketReader {
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<InboundEvent>> {
        let Some(frame) = self.reader.next().await else {
            return Ok(None);
        };
        Ok(Some(match frame? {
            Message::Binary(frame) => self.decode_binary(&frame)?,
            Message::Ping(payload) => InboundEvent::Ping(payload.to_vec()),
            Message::Pong(_) => InboundEvent::Pong,
            Message::Close(_) => InboundEvent::Close,
            Message::Text(_) | Message::Frame(_) => {
                return Err(anyhow::anyhow!(
                    "unexpected non-binary application frame on rkyv session"
                ));
            }
        }))
    }

    fn decode_binary(&self, frame: &[u8]) -> anyhow::Result<InboundEvent> {
        let header = FrameHeaderV1::decode(frame, self.max_payload_bytes)?;
        if !matches!(
            header.kind,
            MessageKindV1::DiscoveryResponse
                | MessageKindV1::DiscoveryChanged
                | MessageKindV1::Ping
                | MessageKindV1::Pong
                | MessageKindV1::SessionError
                | MessageKindV1::ServerDraining
                | MessageKindV1::CommandRequest
        ) {
            return Err(anyhow::anyhow!(
                "unexpected controller message kind on rkyv session"
            ));
        }
        let message = decode_rkyv_frame_v1_on_channel(
            frame,
            self.max_payload_bytes,
            header.kind.logical_channel(),
        )?;
        match message {
            DecodedMessageV1::Ping(ping) => Ok(InboundEvent::ApplicationPing {
                nonce: ping.nonce,
                timestamp_ms: ping.timestamp_ms,
            }),
            other => Ok(InboundEvent::Message(input_from_wire(other)?)),
        }
    }
}
