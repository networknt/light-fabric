# Java backend SDK

Implement `AgentBackend` and call `BackendAdapter.serve`. The adapter owns the
loopback listener, signed-context verification, restart-safe replay file,
bounded requests, fixed routes, identifier equality checks, virtual-thread
dispatch, health endpoints, and SSE framing.
