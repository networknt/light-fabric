# A2A Phase 6 Optional Profile

Phase 6 activates one independently publishable A2A 1.0 profile for external
agents behind `light-a2a`. It contains three separately selectable features:

- an authenticated, policy-bound extended Agent Card;
- allowlisted optional data-only extensions; and
- task-owned push notifications to pre-registered HTTPS callbacks.

The public HTTP+JSON and gRPC bindings, custom public bindings, private-backend
mTLS/gRPC, required extensions, and additional SDK languages remain disabled.
Their activation requires a separate profile and qualification evidence.

`optional-profile.json` is a compiler/runtime contract fixture. An activated
extension URI must have exactly one schema digest and an explicit operation
allowlist. The profile never permits required extensions or an A2A 0.3
projection.

Push delivery uses a Portal-approved callback registration. A request may name
that registration but cannot supply a callback credential or arbitrary URL.
`light-a2a` re-resolves the HTTPS destination, rejects private/non-global
addresses and redirects, signs each attempt with a server-owned HMAC key, and
persists retry, lease, terminal, and dead-letter state in `a2a_ops`.

Rollback disables the binding's Phase 6 selectors and republishes an immutable
snapshot. Existing delivery evidence remains subject to its retention policy;
the runtime stops claiming new deliveries for a disabled or revoked profile.

