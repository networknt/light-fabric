# LLM Dual-Token Phase 1 Contract

Phase 1 introduced the prepared configuration and publication contract from
[LLM User and Agent Authorization](llm-dual-token-authorization.md). Its original
activation guard was removed by [Phase 3](llm-dual-token-phase3.md), after gateway
enforcement and Agent forwarding were implemented. Empty `{}` publications retain
their existing behavior and canonical digests. The issuer and deployment gates
below still apply before activating a nonempty policy.

## Implemented contract

`agent-runtime-protocol::gateway_delegation::GatewayDelegationPolicy` replaces
the untyped `agentPolicy.gatewayDelegation` value. The shape is either exactly
`{}` or the following prepared contract; unknown fields, null `dualToken`, and
incomplete contracts are rejected. The values below are test fixtures, not
qualified deployment credentials or issuer configuration.

```json
{
  "dualToken": {
    "schemaVersion": 1,
    "profile": "user-agent-dual-token-v1",
    "gatewayUrl": "https://llm-gateway:8443/v1",
    "userIssuer": "https://oauth.example.test",
    "userAudience": "urn:com.networknt",
    "workloadIssuer": "https://oauth.example.test",
    "workloadAudience": "urn:com.networknt",
    "tokenEndpoint": "https://oauth.example.test/oauth2/provider/token",
    "clientId": "019d8349-41c6-72ef-95c2-4428a40d0e49",
    "clientSecretFile": "/run/secrets/agent-llm-client-secret",
    "scopes": ["portal.r"],
    "refreshBeforeSeconds": 60,
    "routeAlias": "assistant-dev"
  }
}
```

Both URLs require HTTPS and exclude credentials, query, and fragment.
`gatewayUrl` and `routeAlias` must equal the enclosing immutable model policy.
Both issuer/audience pairs are explicit nonempty exact strings. `clientId` is a
nonnil UUID. `clientSecretFile` is a normalized absolute path beneath
`/run/secrets/`; it names deployment material, never a secret in Config Server.
Scopes are distinct OAuth scope tokens. The refresh margin is 1–599 seconds,
strictly below the current issuer's 600-second client-credentials lifetime.

These checks validate publication structure, not the truth of issuer claims or
live credentials. Phase 2 must validate actual tokens against these expectations.
The contract deliberately has no `ignoreExpiry`, audience bypass, parser fallback,
identity override, or `enabled` switch.

## Publication and digest compatibility

The Java publisher in `light-portal/db-provider` reads the optional instance map
property `agent-policy-authoring.gatewayDelegation`, assigned to the agent's
product version through the existing configuration authoring model. Register
that optional map property and assignment using the existing Portal configuration
commands before authoring a prepared candidate; no new SQL assignment table is
needed. Absent authoring produces `{}`.

`AgentPolicyPublicationPersistence.loadGatewayDelegation` tracks the property,
assignment, instance-value versions, and authored value in
`sourceAggregateVersions.gatewayDelegationAuthoring`. This source is separate
from the generated runtime property, avoiding a publication invalidating its
own inputs. Ambiguous property matches fail. Publication also requires the configured
`clientId` to have an active `auth_client_t` row in the same host with
`api_version_id` equal to the Agent definition UUID. Registration version and
identity enter `sourceAggregateVersions.gatewayClientRegistration`; missing or
mismatched registration denies candidate compilation. `AgentGatewayDelegation` validates
the authored contract before compilation.

`AgentPolicyProjectionCompiler` emits the complete value as one map property,
`agentPolicy.gatewayDelegation`, consumed by the existing `agent.yml` placeholder
`agent.agentPolicy.gatewayDelegation`. It does not flatten child properties.
For nonempty contracts, gateway delegation enters the product-profile digest
material and consequently the policy digest, as well as the complete content
digest. Empty legacy policy material remains byte-compatible. The existing
publication source/content digests detect changes without a parallel version or
assignment store.

Java exports a complete prepared Agent policy fixture, and the Rust consumer
recomputes its canonical content digest. Both runtimes preserve the contract
without adding default fields to `{}`. This is a schema extension requiring the
new consumer; it is not safe to publish to older consumers that ignore the slot.

## Existing alias and identity contract

`LlmModelPersistenceImpl.appendV4Routes` already projects:

| Portal alias visibility | Gateway alias fields |
| --- | --- |
| `PUBLIC` | No internal binding |
| `INTERNAL_AGENT` | `internal: true`, `boundPrincipal: <bound_agent_def_id UUID>` |
| `INTERNAL_WORKLOAD` | `internal: true`, `boundPrincipal: <bound_workload_principal>` |

`instancePropertyCandidateV4` preserves those fields in the aliases map. The
Rust `AliasConfig`, compiler, and runtime already consume them. Phase 1 tests the
real Java route publisher with controlled JDBC rows and feeds its exported
agent-bound alias to the Rust config deserializer. Existing internal-alias
admission and model-listing tests cover rejection/concealment behavior.

For the interactive agent profile, the execution principal must be the existing
agent-definition UUID, not the user JWT's UI client ID and not necessarily the
service-token client ID. Resolve the latter through the existing host-scoped
`auth_client_t.api_version_id` registration and active Agent definition. Do not accept an authored or caller-supplied UUID
as identity proof. That verification and adapter are Phase 2 work.

The binding, limiter, billing, and `routeAlias` rules in the parent design remain
normative. The prepared `routeAlias` is an additional immutable restriction and
cannot replace or widen signed token restrictions. This phase adds no new alias
or permission store and changes no live execution principal.

## Concrete issuer acquisition and renewal contract

The checked `portal-service/apps/light-oauth/src/main.rs` implements
`POST /oauth2/{providerId}/token` with `grant_type=client_credentials` and supports
`client_secret_basic` and `client_secret_post`. The selected client contract uses
HTTP Basic client authentication with `clientId` and the secret read from
`clientSecretFile`, and a form body containing `grant_type=client_credentials`
and the space-joined requested scopes. Use the configured HTTPS endpoint and
trusted CA; disallow redirects. Never request the `long_lived` grant.

The current normal access token lasts 600 seconds and has no refresh token.
Renewal means repeating the client-credentials grant, not using `refresh_token`.
The Phase 3 provider must read the current mounted secret on acquisition, refresh
before the configured margin, single-flight concurrent refreshes, validate the
returned token before cache replacement, and retain the previous verified token
only until its expiry. Bound backoff to the remaining valid lifetime; after
expiry fail new inference explicitly, then recover when acquisition succeeds.
A static `registry_token` is not a renewable inference credential.

Issuer limitations are concrete deployment gates:

- `generate_jwt_with_key` uses the server's configured `jwt_audience`; the
  client-credentials form does not choose a per-request audience. Qualify that
  issuer-wide audience as an accepted gateway recipient for both user and
  workload profiles. `urn:com.networknt` is not proof of that deployment decision.
  If a distinct gateway-only audience is required, issuer changes or a separate
  qualified issuer configuration are required before activation.
- At the Phase 1 baseline, `handle_client_credentials` passed empty extra claims. It did not emit agent
  definition, host, environment, or `routeAlias` claims from custom registration
  claims on this grant. The target workload contract requiring host/environment
  therefore needed an issuer change that derives them from trusted registration;
  [Phase 4](llm-dual-token-phase4.md) adds that registered workload projection.
  Do not let client-supplied form claims assert them. Existing signed alias bounds
  must likewise survive migration, or have a separately qualified equivalent.
- `sub` and `client_id` represent the OAuth client. They are not the Portal agent
  definition UUID. Verify the canonical mapping before using the alias binding.

Phase 1 specifies these limits; it does not claim the current issuer can already
issue a deployable token satisfying every gate. Issuer changes and live token
qualification must precede Phase 4 activation, without relaxing the contract.

## Concrete user reauthentication contract

`portal-view/src/pages/genai/Chat.tsx` opens the WebSocket using the browser's
access-token cookie and CSRF subprotocol. `light-agent` authenticates at upgrade
and captures the principal/token in `handle_socket`; subsequent turns do not
refresh them. `UserContext.tsx` has an HTTP-based cookie-renewal path, but no chat
reauthentication protocol exists yet.

Phase 3 must check captured-token expiry before each model call and stop new
admission when it expires. Use a structured `authentication_required` chat event
and close code `4401` as the new protocol contract. The UI renews authentication
through the existing HTTP login/refresh-cookie flow, then reconnects with the
same session ID and fresh cookies/CSRF context. If renewal fails, require sign-in.
Never carry an old captured bearer into the reconnected socket.

The agent revalidates host, user, and agent ownership on reconnect. Preserve
accepted turn IDs and query/reconcile their durable status; do not automatically
resubmit an accepted or ambiguously completed turn. Unaccepted drafts may remain
in the UI for explicit submission. A tool-loop call encountering expiry ends
that turn with an explicit authentication outcome; reconnect does not replay
its side effects. An already admitted stream may finish under its admission
context, but a later model call requires valid credentials.

These requirements are implemented and covered by the Phase 3 qualification gate.
Full deployed-path qualification remains Phase 4.

## Verification

Run `scripts/run-agent-llm-phase1-gates.sh` from `light-fabric`. It runs the Java
contract/publication tests, compares regenerated Java fixtures with checked-in
Rust fixtures, tests Rust contract and agent validation, tests alias parsing and
existing binding/concealment behavior, and builds the book.

JDBC publication tests use controlled mocks; they prove compiler/source tracking
and serialization contracts, not a live database migration or deployment. Phase 2
still requires real audit persistence, and Phase 4 requires live issuer, database,
and UI qualification. Phase 1 changes neither live configuration nor credentials.
