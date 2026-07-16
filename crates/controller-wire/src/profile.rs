use crate::WireError;

pub const MCP_JSON_V1: &str = "light-controller-mcp-json-v1";
pub const RUNTIME_JSON_V1: &str = "light-controller-runtime-json-v1";
pub const RUNTIME_RKYV_V1: &str = "light-controller-runtime-rkyv-v1";
pub const PROFILE_MAJOR_V1: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalChannel {
    McpSession,
    SessionControl,
    Command,
    RuntimeEvents,
}

impl LogicalChannel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::McpSession => "mcp-session",
            Self::SessionControl => "session-control",
            Self::Command => "command",
            Self::RuntimeEvents => "runtime-events",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageKindV1 {
    ClientHello = 1,
    ServerHello = 2,
    MetadataUpdate = 3,
    DiscoveryRequest = 4,
    DiscoveryResponse = 5,
    DiscoveryChanged = 6,
    Ping = 7,
    Pong = 8,
    SessionError = 9,
    ServerDraining = 10,
    CommandRequest = 100,
    CommandResponse = 101,
    RuntimeNotification = 200,
}

impl MessageKindV1 {
    pub const fn logical_channel(self) -> LogicalChannel {
        match self {
            Self::CommandRequest | Self::CommandResponse => LogicalChannel::Command,
            Self::RuntimeNotification => LogicalChannel::RuntimeEvents,
            _ => LogicalChannel::SessionControl,
        }
    }

    pub fn require_channel(self, channel: LogicalChannel) -> Result<(), WireError> {
        if self.logical_channel() == channel {
            Ok(())
        } else {
            Err(WireError::InvalidLogicalChannel {
                kind: self as u16,
                channel: channel.as_str(),
            })
        }
    }
}

impl TryFrom<u16> for MessageKindV1 {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::ClientHello),
            2 => Ok(Self::ServerHello),
            3 => Ok(Self::MetadataUpdate),
            4 => Ok(Self::DiscoveryRequest),
            5 => Ok(Self::DiscoveryResponse),
            6 => Ok(Self::DiscoveryChanged),
            7 => Ok(Self::Ping),
            8 => Ok(Self::Pong),
            9 => Ok(Self::SessionError),
            10 => Ok(Self::ServerDraining),
            100 => Ok(Self::CommandRequest),
            101 => Ok(Self::CommandResponse),
            200 => Ok(Self::RuntimeNotification),
            other => Err(WireError::UnknownMessageKind(other)),
        }
    }
}
