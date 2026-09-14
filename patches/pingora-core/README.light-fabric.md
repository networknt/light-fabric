# light-fabric Pingora core patch

This directory vendors crates.io `pingora-core` 0.8.1 under its Apache-2.0
license. It adds an opt-in HTTP/1 request initiation guard for A2. This is a
transport primitive, not a claim that Gateway's A2 route integration is complete.

The guard is installed only after upstream connection and TLS preparation. The
HTTP/1 header write scopes the operation; the TCP socket `poll_write` and
`poll_write_vectored` enforce it underneath TLS and buffering. The synchronous
first-write callback owns the deadline check and state transition. A persisted
socket fence prevents cleanup from flushing expired or cancelled request bytes.
All socket request writes remain bounded by the initiation deadline, including
TLS control records and later body writes. This is deliberately conservative;
response reads can continue beyond the deadline. There is no
reset of the action guard for a retry.

Supported guarded transport is HTTP/1 over the concrete Pingora TCP transport,
optionally wrapped by Pingora rustls. Unknown/custom transports and HTTP/2 do not
qualify. Ordinary `write_request_header` retains upstream behavior. The original
upstream examples and certificates are test fixtures, not deployment credentials.

Targeted tests:

```sh
cargo test -p pingora-core --lib --features rustls request_write_guard
cargo test -p light-pingora --lib guarded_http::tests
```

The tests cover pending writes, cancellation, real connection reuse, TLS-buffered
headers after expiry, and the single-attempt adapter's partial-send failure.
They do not replace Gateway proxy-path, mTLS route, or multi-replica qualification.

Before upgrading, diff against the matching crates.io source, review every
buffer/TLS/socket write boundary, and rerun transport and proxy regression tests.
