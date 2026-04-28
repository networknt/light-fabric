# MCP Client

`mcp-client` is a client for calling MCP-compatible gateway endpoints.

It provides a small API for listing and invoking tools through a configured MCP
gateway path. It is intentionally focused on the client side; MCP server
implementations live in applications or framework layers.

## Main Types

- `McpGatewayClient`: gateway client used by applications.
- `McpTool`: tool metadata returned by the gateway.
- `McpContent`: content item returned by MCP tool calls.
- `McpToolCallResult`: structured result for a tool invocation.

## Usage

```rust
use mcp_client::McpGatewayClient;

let client = McpGatewayClient::new(gateway_url, path, timeout_ms);
let result = client.call_tool("tool.name", arguments).await?;
```

## Consumers

`light-agent` uses this crate when an agent session needs to discover or invoke
tools exposed through an MCP gateway.
