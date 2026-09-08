# Agent LLM dual-token Phase 3 implementation

Phase 3 implements request-scoped Agent forwarding, renewable workload credentials,
and interactive reauthentication. A validated nonempty
`agentPolicy.gatewayDelegation.dualToken` now selects the implementation; `{}`
retains the legacy single-token path. Deployment and live issuer qualification
remain Phase 4. No live configuration has been activated by this implementation.

## Request-scoped forwarding

`CompatibleProvider` accepts a typed `GatewayAuthorization` for one turn. The
Agent constructs it only from the authenticated invocation and its published
workload credential source. Each model call, including tool-loop continuations,
requests the credentials again and checks both expiry times immediately before
sending. It sends the original user JWT in `Authorization` and the acquired
workload JWT in `X-Scope-Token`, both with the Bearer scheme.

The shared HTTP client never holds user credentials in default headers. Concurrent
turns have separate user authority; only the workload cache is shared. The provider
pins its configured destination and rejects a changed base URL. Agent outbound
clients disable redirects, and activating this profile requires TLS hostname
verification and real JWT verification. Incoming duplicate or malformed user
credential headers are rejected. UI-supplied scope credentials are ignored.

Credentials are not serialized into conversation history, turn records, snapshots,
or audit. Credential-bearing request headers are marked sensitive, authenticated
request objects have no derived Debug output, and dual-token gateway errors do not
include response bodies that could echo credentials. The registry credential remains
separate. Native A2A inference explicitly rejects this profile because it has no
original interactive user token; there is no service-token fallback.

The current compatible client uses buffered chat completions. This phase preserves
that transport and its tool-loop behavior; it does not add an Agent streaming API.
No automatic retry is added after a potentially billable request.

## Workload issuance and renewal

`gateway_credentials.rs` implements the Phase 1 client-credentials contract:
HTTP Basic client authentication, form `grant_type=client_credentials`, and the
published space-separated scopes. It uses the configured endpoint, trusted CA,
a 15-second timeout, no redirects, and a bounded response body. Every acquisition
reopens `clientSecretFile`, allowing atomic mounted-secret rotation.

The returned JWT passes the shared signature verifier and independent checks for
issuer, audience, expiry, not-before/issued-at, registered client ID, host,
environment, required scopes, and any signed route-alias restriction. Issued
lifetime must exceed the refresh margin. Legacy expiry bypass cannot extend it.

A mutex makes refresh single-flight. Calls reuse the verified cache until the
refresh margin. A failed refresh may use the previous verified token only until
its original expiry; failures have a two-second retry backoff. Initial acquisition
failure or expiry returns `workload_credential_unavailable` without dispatch.
A later successful grant recovers the cache. An expired captured user token fails
before workload acquisition, and expiry is checked again after acquisition so
waiting for refresh cannot extend user authority.

## Interactive expiry and reconnect

WebSocket upgrade verifies the current user JWT, its explicit gateway audience,
and host/user/Agent ownership. Under this profile the Agent advertises an
`authentication_context` event containing only `expiresAt`. It checks captured
expiry before durable turn admission and before every outbound model call.

Expiry emits `authentication_required` with the client message ID and whether
the turn was already admitted, followed by close code 4401. An admitted turn that
expires during execution terminates with that authentication outcome. Reconnection
does not replay earlier tool side effects or model requests.

The UI uses the existing same-origin HTTP refresh-cookie probe, then reconnects
with fresh cookies and CSRF context under the same session ID. Renewal is bounded
to 15 seconds and cancelled on disconnect, unmount, or identity/deployment selection
change. Cookie owner/host changes block reconnect; the Agent independently checks
durable ownership. Renewal failure requires sign-in. An expired unsent draft
remains for explicit submission after reconnect.

Ordinary chat now supplies `clientMessageId` as coding already did. The Agent
acknowledges admission with `turnAccepted`. The UI retains submitted message IDs
and accepted turn IDs, without credentials, in the existing per-user/deployment
session-storage namespace. Reconnect reports the latest 100 durable turn states
through `turn_status`; older or unavailable status stays explicitly unresolved.
Accepted or ambiguous turns are never automatically resubmitted. An explicitly
unadmitted message retains its ID when restored as a draft. Existing database
idempotency and session ownership remain authoritative.

## Verification and remaining deployment gates

Run `scripts/run-agent-llm-phase3-gates.sh` with
`LIGHT_AGENT_TEST_DATABASE_URL` pointing to a disposable PostgreSQL database with
pgvector, operational metadata migrations, and Agent store migrations applied.
The script requires the database and selects `agent_ops`; it does not silently
skip the database tests. `PORTAL_VIEW_SOURCE_DIR` can override the sibling UI path.

The passing gate covers:

- Signed JWT issuance through a mock HTTP issuer/JWKS server, concurrent refresh,
  secret rotation, actual expiry, failed-refresh fallback, expiry denial, recovery,
  and isolation of users sharing the workload cache.
- Real HTTP model requests with both headers, credential rechecks, registry-token
  exclusion, destination changes, redirect rejection, and legacy compatible-client
  regressions.
- Agent configuration, ownership and authentication-event tests; real PostgreSQL
  same-owner resume, different-owner rejection, duplicate admission, FIFO ordering,
  and unavailable-execution reconciliation.
- UI reconnect, retained drafts and accepted IDs, durable status reconciliation,
  account-change rejection, cancellation on Host change, and existing chat tests.
- Existing gateway data-plane regressions, UI lint, documentation build, and diff checks.

The issuer fixtures use loopback HTTP only in tests; production publication still
requires HTTPS. These checks are not a live browser-to-Portal-to-Agent-to-gateway
qualification. Phase 4 must qualify the deployed issuer, CA, audience acceptance,
registration claims, mounted secret rotation, gateway audit sink, and reconnect
behavior together. The issuer limitations recorded in Phase 1 remain hard gates:
missing workload host/environment or an unsuitable user audience must be fixed at
issuance, without disabling validation.
