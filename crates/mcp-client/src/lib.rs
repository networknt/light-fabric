pub mod client;
pub mod protocol;
pub mod wire;

pub use client::{McpGatewayClient, McpProfile};
pub use protocol::{McpContent, McpTool, McpToolCallResult};
