# How to Call an MCP Server with Curl

To talk to an HTTP-based Model Context Protocol (MCP) server using curl, you must follow the strict JSON-RPC 2.0 lifecycle defined by the spec. This includes initiating a handshake, completing an initialization confirmation, and executing the actual tool call.

Here is the exact multi-step process required to interact with a streamable HTTP or Server-Sent Events (SSE) MCP server.

## 1. Initialize the Connection

Every MCP interaction requires a handshake. You must send an `initialize` method to create your session.

```bash
curl -s -i -X POST "https://your-mcp-server.example.com/mcp" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}'
```

**Action**: Extract the `mcp-session-id` from the response headers and export it (e.g., `export SESSION_ID="..."`).

## 2. Confirm Initialization

Send an `initialized` notification to finalize setup.

```bash
curl -s -X POST "https://your-mcp-server.example.com/mcp" \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: $SESSION_ID" \
  -d '{"jsonrpc": "2.0", "method": "initialized", "params": {}}'
```

## 3. List and Call Tools

Use `tools/list` to find available tools, and `tools/call` to execute them, ensuring arguments are structured correctly.

### List Tools

```bash
curl -s -X POST "https://your-mcp-server.example.com/mcp" \
  -H "Mcp-Session-Id: $SESSION_ID" \
  -d '{"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}'
```

### Call Tool

```bash
curl -s -X POST "https://your-mcp-server.example.com/mcp" \
  -H "Mcp-Session-Id: $SESSION_ID" \
  -d '{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "...", "arguments": {}}}'
```

## Tips

*   **Auth**: Add `-H "Authorization: Bearer $TOKEN"` for protected servers.
*   **Streaming**: Use `curl -N` for SSE endpoints.
