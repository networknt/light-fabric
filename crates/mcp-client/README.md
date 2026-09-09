# MCP HTTP client

`McpGatewayClient::new(url)` selects `2026-07-28` Streamable HTTP. It supplies
modern metadata and routing/parameter headers, discovers the server, and loads
validated tool definitions before calling. Invalid header definitions are
excluded. JSON and request-scoped SSE responses require an exact JSON-RPC ID.
Only complete tool results are supported; MRTR and independent server requests
fail explicitly. Structured content, output schemas, cache fields and content
annotations remain available in the models.

For a legacy target, select `with_profile(McpProfile::Legacy20251125)` before
use. Explicit June/March 2025 profiles are also available. Legacy requests
initialize once per credential partition and send initialized plus session and
version headers. Call `close(auth_header)` to DELETE that partition's session
and clear its caches. A session-bound HTTP 404 also clears the session and
cache; the failed operation is returned without replay, and the next explicit
operation initializes a new session. There is no 2024 adapter and no automatic fallback/replay.
The light-agent template explicitly selects November 2025 while gateway modern
support remains opt-in; set `mcp-client.protocolVersion: 2026-07-28` only for a
qualified target.

The caller supplies the resource's Bearer credential on every operation. This
module does not implement authorization-code, registration or token refresh.
It does not follow redirects or expose arbitrary HTTP error bodies. Caches are
partitioned by a SHA-256 credential digest, retain at most 32 credential
partitions, cap TTL at five minutes, catalogs at 4096 tools and pagination at 64
pages. Idle partitions are reclaimed in least-recently-used order; active and
queued operations cannot be evicted. If all 32 partitions are busy, admission
fails transiently until an operation completes. Eviction clears local state;
legacy server sessions expire under the server policy, so callers can use
`close` for immediate DELETE cleanup while the credential is still available.
Each credential has its own lifecycle lock: unrelated credentials perform HTTP
requests concurrently, while one credential serializes initialization, calls
and cleanup. Legacy calls initialize as needed and invoke directly; only an
explicit `list_tools` fetches the legacy catalog. Bodies default to
4 MiB and requests retain the configured timeout. No failed mutation is retried.
