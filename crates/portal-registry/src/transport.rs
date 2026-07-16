use url::Url;

use crate::candidate::{ConnectionFailure, ControlCandidate};
use crate::logical::RuntimeSessionOutput;
use crate::protocol::{RegistrationResponse, ServiceRegistrationParams};
use crate::rkyv_websocket::{RkyvWebSocketAdapter, RkyvWebSocketReader, RkyvWebSocketWriter};
use crate::websocket::{InboundEvent, WebSocketAdapter, WebSocketReader, WebSocketWriter};

pub(crate) enum RegistryTransport {
    Legacy(WebSocketAdapter),
    Rkyv(RkyvWebSocketAdapter),
}

impl RegistryTransport {
    pub(crate) async fn connect(
        controller_url: &Url,
        connector: Option<tokio_tungstenite::Connector>,
        candidate: ControlCandidate,
        service_jwt: &str,
    ) -> Result<Self, ConnectionFailure> {
        match candidate {
            ControlCandidate::LegacyJson => Ok(Self::Legacy(
                WebSocketAdapter::connect(controller_url, connector, candidate, None).await?,
            )),
            ControlCandidate::RuntimeRkyvV1 => Ok(Self::Rkyv(
                RkyvWebSocketAdapter::connect(controller_url, connector, service_jwt).await?,
            )),
        }
    }

    pub(crate) async fn register(
        &mut self,
        params: ServiceRegistrationParams,
    ) -> anyhow::Result<RegistrationResponse> {
        match self {
            Self::Legacy(adapter) => adapter.register(params).await,
            Self::Rkyv(adapter) => adapter.register(params).await,
        }
    }

    pub(crate) fn split(self) -> (RegistryWriter, RegistryReader) {
        match self {
            Self::Legacy(adapter) => {
                let (writer, reader) = adapter.split();
                (
                    RegistryWriter::Legacy(writer),
                    RegistryReader::Legacy(reader),
                )
            }
            Self::Rkyv(adapter) => {
                let (writer, reader) = adapter.split();
                (RegistryWriter::Rkyv(writer), RegistryReader::Rkyv(reader))
            }
        }
    }
}

pub(crate) enum RegistryWriter {
    Legacy(WebSocketWriter),
    Rkyv(RkyvWebSocketWriter),
}

impl RegistryWriter {
    pub(crate) async fn send_message(
        &mut self,
        message: RuntimeSessionOutput,
    ) -> anyhow::Result<()> {
        match self {
            Self::Legacy(writer) => writer.send_message(message).await,
            Self::Rkyv(writer) => writer.send_message(message).await,
        }
    }

    pub(crate) async fn send_ping(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Legacy(writer) => writer.send_ping().await,
            Self::Rkyv(writer) => writer.send_ping().await,
        }
    }

    pub(crate) async fn send_pong(&mut self, payload: Vec<u8>) -> anyhow::Result<()> {
        match self {
            Self::Legacy(writer) => writer.send_pong(payload).await,
            Self::Rkyv(writer) => writer.send_pong(payload).await,
        }
    }

    pub(crate) async fn send_application_pong(
        &mut self,
        nonce: u64,
        timestamp_ms: i64,
    ) -> anyhow::Result<()> {
        match self {
            Self::Legacy(_) => Err(anyhow::anyhow!(
                "application ping is invalid on a legacy transport event"
            )),
            Self::Rkyv(writer) => writer.send_application_pong(nonce, timestamp_ms).await,
        }
    }
}

pub(crate) enum RegistryReader {
    Legacy(WebSocketReader),
    Rkyv(RkyvWebSocketReader),
}

impl RegistryReader {
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<InboundEvent>> {
        match self {
            Self::Legacy(reader) => reader.next().await,
            Self::Rkyv(reader) => reader.next().await,
        }
    }
}
