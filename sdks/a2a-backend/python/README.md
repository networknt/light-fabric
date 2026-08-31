# Python backend SDK

Implement the `AgentBackend` callbacks and call `serve`. The SDK owns the
loopback listener, signed-context verification, restart-safe replay protection,
identifier equality checks, bounded input, health routes, and SSE framing.
Business code receives a typed JSON request and verified context, never a
caller token or Portal policy document.
