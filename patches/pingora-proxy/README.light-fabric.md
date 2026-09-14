# light-fabric Pingora proxy patch

This directory vendors `pingora-proxy` 0.8.1 from crates.io under its original
Apache-2.0 license. The workspace also patches `pingora-core` 0.8.1 for the
opt-in A2 socket write guard; other Pingora crates remain pinned to 0.8.1.

`ProxyHttp::prebuffered_request_body` lets an application return a fully consumed,
bounded body after pre-upstream authentication. It still passes through the
normal `request_body_filter`. Ordinary routes retain their existing retry
behavior and the callback defaults to `None`.

`ProxyHttp::upstream_request_write_guard` optionally supplies a one-shot action
guard. HTTP/1 passes it into the patched core's guarded header operation. The
core enforces the guard at the TCP socket beneath TLS/buffering. Guarded HTTP/2
and custom-protocol dispatch fail before operation headers are sent. The default
`error_while_proxy` does not enable reused-connection retries for guarded routes.
Applications must keep the same guard across all retries; their own retry hooks
must not enable another action initiation. Gateway integration and its effective
retry settings still require separate A2 qualification.

When upgrading Pingora, compare both vendored crates with upstream and rerun
`scripts/run-hmac-phase0-gates.sh`, the core `request_write_guard` tests, and the
light-pingora `guarded_http` tests. Review any changed TLS/buffering/write path;
a high-level send callback is not an equivalent replacement for the socket hook.
