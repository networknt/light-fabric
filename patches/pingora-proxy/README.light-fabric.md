# light-fabric Pingora proxy patch

This directory vendors `pingora-proxy` 0.8.1 from crates.io under its original
Apache-2.0 license. The workspace `[patch.crates-io]` entry selects it while all
other Pingora crates remain pinned to 0.8.1.

The light-fabric delta is intentionally limited to one optional
`ProxyHttp::prebuffered_request_body` callback and its HTTP/1.1 and HTTP/2 call
sites. The callback lets an application return a fully consumed, bounded body
after pre-upstream authentication. The bytes still pass through the normal
`request_body_filter` and can be replayed for an upstream retry. Existing proxy
implementations receive the default `None` behavior.

When upgrading Pingora, compare the vendored source with the matching upstream
release, reapply only this callback if upstream still lacks an equivalent, and
run `scripts/run-hmac-phase0-gates.sh` before changing the workspace patch.
