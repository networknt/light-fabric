# LLM User and Agent Authorization

## Policy lifetime

Published Agent configuration and derived gateway assignments remain valid until
replaced or explicitly revoked through an applied configuration update. They do
not expire with elapsed time. `PERSISTENT` is the publication default; legacy
`BOUNDED` and `LOCAL_DEMO` inputs remain accepted for compatibility, but new
publications do not emit `runtimePolicy.refreshAfter` or `runtimePolicy.expiresAt`.
Updated runtimes accept old snapshots containing these fields without enforcing
them. Gateway assignment `expiresAt` is likewise a legacy field only.

For disconnected operation, download the complete validated `values.yml` and
mount it read-only under an operator-controlled configuration directory. Disable
remote bootstrap/controller registration for a deliberately disconnected
deployment. Keep the file immutable until an intentional update; an embedded
content digest checks consistency but is not a trusted signature against an actor
who can rewrite both the content and its digest. File ownership and read-only
mounts provide the deployment boundary.

During Portal or Config Server downtime, keep the accepted configuration. Remote
bootstrap already falls back to available local/cached values on connection or
HTTP failure; persist that cache if restart recovery is required. A restart without
any complete local configuration still cannot bootstrap during an outage.
Identity, digest, schema, valid-from, and rollback checks continue to apply.
Explicit revocation takes effect when delivered and applied; disconnected Agents
cannot discover undelivered revocations.

This lifetime applies only to configuration. User and workload JWT expiry,
temporary delegation grants, execution deadlines, and session limits remain
enforced. OAuth token issuance, model providers and operational stores remain
runtime dependencies; removing policy expiry does not make those services offline.
Upgrade both runtimes and the Portal publisher before emitting expiry-free
snapshots: older binaries require/enforce the legacy fields.

Status: Phases 1–3 implemented; deployment qualification remains Phase 4.
The observations under "Current implementation and gaps" describe the original
September 7, 2026 baseline. Linked phase reports document the implemented changes.

## Problem and decision

An interactive chat connection can succeed while its first model request fails
with `403 permission_denied`. The browser authenticates to `light-agent` as a
user, but the current agent model client sends the agent service token as
`Authorization` to `llm-gateway`. An endpoint protected by
`req-access-light-portal.lightapi.net` can require `admin` or `host-admin`, which
the service token does not contain.

Preserve the original user access token in `Authorization` and send the agent's
own service token in `X-Scope-Token`. The gateway must independently validate
both tokens, authorize the user, enforce applicable agent-to-model assignments,
and audit both identities. Do not grant administrator roles to service tokens
or disable endpoint access control to make interactive inference work.

## Scope

This design covers interactive user-delegated model calls from `light-agent`
to `llm-gateway`, including subsequent model calls within a tool loop, retries,
and streaming admission. The same contract should apply to other generation
endpoints when they support agent-mediated requests.

Existing direct-user inference and separately authorized workloads, such as
Knowledge embeddings, retain explicitly defined admission profiles. Background
jobs and A2A invocations without an original user access token require a
separate delegation design; they must not fabricate a user identity or fall
back to presenting the service token as the user.

## Current implementation and gaps

| Area | Current behavior | Required change |
| --- | --- | --- |
| `apps/light-agent/src/main.rs` | `build_model_provider` passes the service token to `CompatibleProvider`; authenticated user authorization is retained for MCP calls | Carry current-turn user authorization into model calls and attach the service token separately |
| `crates/model-provider/src/compatible.rs` | `send_request` places its configured API key in `Authorization` | Provide a typed, request-scoped gateway authentication path that can send both headers |
| `frameworks/light-pingora/src/security.rs` | JWT verification selects `Authorization`, falling back to `X-Scope-Token` | Explicitly verify both tokens for dual-token requests; the fallback is not dual validation |
| `frameworks/light-pingora/src/access_control.rs` | Endpoint rules evaluate the primary principal | Keep the verified user as the primary principal and expose verified workload context separately |
| `apps/light-gateway/src/main.rs` | `llm_billing_context` derives principal, billing subject, and optional `routeAlias` from one principal | Separate user, workload, authorization, and accounting identities |
| `crates/llm-gateway/src/http.rs` | `bound_model_alias` constrains a signed route alias | Add authoritative agent assignment enforcement; alias equality alone is insufficient |

These observations do not establish that the existing control-plane binding
projection contains all required workload identity and assignment fields.
Inventory that projection and its canonical schema before implementing it.

## Existing delegation and selected tradeoff

`crates/agent-delegation` already defines signed `lad1` credentials with caller
claims, agent actor/definition, audience, expiry, policy digests, and replay IDs.
The gateway's `authenticate_agent_delegation` verifies them and consumes their
replay identity through shared storage. Its constructed principal deliberately
retains both `client_id = agent_actor` and `user_id = caller_subject`.

The current kinds are tools-list, tool-call, Knowledge-retrieve, and
Knowledge-upload; there is no LLM inference kind. Knowledge currently mints
60-second tokens; the verifier accepts a maximum lifetime of 300 seconds.
The current agent LLM path uses its registry credential, not a 60-second LLM
delegation. Therefore this proposal does not replace an already qualified LLM
delegation path, but it must account for the existing delegation infrastructure.

The selected user-requested contract forwards the original user JWT. This
exposes a reusable, potentially broad-authority bearer credential to another
service and is weaker in containment than an audience-bound, single-use
credential. TLS, redaction, and destination restrictions do not make those
properties equivalent. Confirm that the user's token issuer authorizes the LLM
gateway as a recipient; do not disable audience checking to permit forwarding.
If that contract cannot be met, block this profile and design an exchange using
the existing delegation infrastructure, rather than silently changing token
semantics. Extending `DelegationKind` for LLM operations would require explicit
operation/alias binding and replay/retry semantics; it is not a second parallel
mechanism to add in this implementation.

Existing tools/Knowledge delegation remains unchanged. For the new LLM profile,
reject `lad1` credentials before generic delegation authentication consumes
replay state: neither existing tool nor Knowledge authority authorizes LLM
inference. Test this route/profile separation explicitly.

`agentPolicy.gatewayDelegation` already exists as an untyped `Value` and has no
runtime consumer in the agent. Type and validate this existing projection slot
for per-agent gateway policy, trusted destination, credential source, and claim
requirements. Do not introduce a parallel per-agent configuration surface or
put bearer tokens in published policy. Schema changes must participate in the
existing immutable snapshot/digest and publisher compatibility workflow.

## Request and trust contract

```text
UI -> light-agent
  Authorization: Bearer <original user access token>

light-agent -> llm-gateway
  Authorization: Bearer <original user access token>
  X-Scope-Token: Bearer <agent service token>

llm-gateway -> inference provider
  Provider-specific credentials owned by llm-gateway
```

Neither inbound token is forwarded to the inference provider. Request bodies,
prompts, tools, and caller-supplied identity headers cannot override either
verified principal. The agent ignores any UI-supplied `X-Scope-Token` and injects
its configured service credential.

Maintain separate typed user and workload principals. The user principal
contains the verified user identifier, including the deployment's established
`uid` mapping, roles, issuer, and host. The workload principal contains the
verified service subject/client identity, issuer, host, and applicable audience,
environment, and scopes. Never merge workload claims into user roles.

Reuse the existing control-plane agent/service identity and alias binding
projection to resolve the workload to its agent definition within the
host/environment. Inventory the exact canonical identity format before coding;
do not introduce a new workload mapping store. An
arbitrary request `agentDefId` is not evidence of identity. If one credential
maps to multiple logical agents, fail ambiguous resolution until a trusted,
verifiable discriminator is defined; a caller-selected agent ID is insufficient.

## Agent request lifecycle

Capture the original access token from the authenticated invocation and pass it
through the current turn's model request context. Keep it out of shared client
default headers and globally cached provider objects. A provider created for one
turn may retain that turn's credentials only for its bounded lifetime.

Every tool-loop continuation and retry carries the same explicit user/workload
pair. Today `authenticate_request` runs at WebSocket upgrade, and `handle_socket`
retains that `AuthenticatedRequest` for subsequent turns. A new turn on the same
socket does not refresh authentication. Implement UI token renewal followed by
reauthenticated reconnect before accepting another turn after expiry. Preserve
session ownership and queued-turn identifiers across reconnect, reject identity
changes for an existing session, and prevent duplicate turn submission. Check
expiry before every outbound model call; renewal cannot extend an already
captured token. An in-band replacement protocol is outside this initial scope. Tokens are
not persisted in conversation history, prompts, checkpoints, logs, or audit
records. Gateway destinations remain trusted configuration; redirects must not
forward either credential to another origin.

If the user token expires before a later model call, reject that call and let
the UI renew authentication through its established flow. An open chat
connection does not extend the token's authority. Do not automatically repeat
a potentially billable inference after ambiguous completion.

## Workload credential issuance and renewal

The current agent credential comes from `registry_token` and is retained in
`AppState.llm_gateway_token` at startup. It is not evidence of a gateway-audience
credential, and a static expired token cannot recover through retries.

Before enabling this profile, implement an issuer-supported service credential
acquisition and refresh path. Obtain a token with the gateway's accepted audience,
agent identity, host/environment, and required scopes; cache it until a safety
margin before expiry, refresh with bounded concurrency, and rotate the credential
used for acquisition. Existing valid cached credentials may serve only until
expiry. After expiry or failed initial acquisition, stop new inference with an
explicit credential-unavailable outcome; do not retry the stale startup token or
turn off expiry checks. Keep registry authentication separate unless its issued
token is explicitly qualified for both purposes.

The issuer endpoint, supported grant, audience value, and source of refresh
credentials are Phase 1 deliverables, not assumptions that current registry
configuration already satisfies. Prove renewal across an actual expiry boundary
before rollout.

## Gateway admission sequence

Retain `X-Scope-Token` to preserve the requested header contract and avoid a new
header allowlist/configuration surface. A distinct header would remove the need
to distinguish its two wire forms, but this design accepts that compatibility
cost explicitly. Select parsing behavior from trusted route configuration, not
from caller input or a fallback after failed verification.

1. Select a route admission profile and parse headers before authentication.
   The current `request_header` accessor returns one value and cannot establish
   uniqueness. Inspect all values from the underlying header map and reject
   duplicates, comma-combined credentials, malformed values, and conflicts.
   Add a shared tested Bearer parser for this dual-token path: the current scope
   fallback passes its header directly to JWT verification without removing
   `Bearer`. Do not route the new wire form through that fallback. Explicitly
   preserve and regression-test legacy raw scope-token profiles separately.
2. Validate the user token and, when supplied or required, the workload token
   independently: signature, trusted issuer, expiry/not-before, applicable
   audience, and required identity claims. A supplied invalid scope token is
   never ignored because the user token is valid.
3. Resolve and validate the workload identity. Check its required scope,
   environment, and host against the user and resource host. Audience and
   environment requirements must come from an explicit issuer/profile contract.
4. Evaluate `req-access-light-portal.lightapi.net` against the user principal.
   A valid agent credential cannot compensate for a denied user.
5. Parse and resolve the requested model under the active LLM policy snapshot.
   Apply any existing signed `routeAlias` restriction as an additional bound.
6. Build the explicit execution identity described below and evaluate existing
   internal-alias binding with it; do not derive it from the user JWT.
7. Record the admission decision and its policy revision, then enter the
   existing quota, budget, routing, and inference path.

Protected routes must not permit mock identity or expiry bypass in qualification
or production. In particular, deployments currently using
`security.ignoreJwtExpiry: true` need current credentials and enforced expiry
before this contract can be qualified.

Keep the existing primary-principal API compatible where possible, adding a
separate verified workload context for LLM admission rather than changing the
meaning of `AuthPrincipal` for unrelated handlers.

## Execution identity and existing alias bindings

The runtime already implements assignment enforcement through alias `internal`
and `boundPrincipal`. The compiler requires a principal for an internal alias;
buffered generation, streaming, embeddings, probes, and model listing use that
binding. Reuse these fields and the existing control-plane publication path.
Do not add a second assignment database or a competing runtime permission map.
Phase 1 verifies the publisher's exact agent identity representation and missing
wiring; it does not recreate the already implemented binding mechanism.

Changing only `Authorization` is unsafe: `llm_billing_context` currently prefers
`client_id`, and its `principal_id` drives internal alias access, per-principal
concurrency/rate buckets, and default billing. Use an explicit adapter:

| Consumer | Dual-token source |
| --- | --- |
| Endpoint RBAC/CEL and user audit | Verified user principal; preserve established `uid`/role mapping |
| Internal alias matching and runtime `principal_id` | Verified agent identity in the exact existing `boundPrincipal` representation |
| Per-principal limits | Same canonical agent execution principal as binding; retain configured limits |
| Billing subject | Existing trusted agent billing attribution and fallback to execution principal; no automatic switch to UI client or human |
| Workload audit | Verified service identity plus authoritatively resolved agent definition |
| `bound_model_alias` | Effective verified route restriction, independent of primary RBAC principal |

Never let untrusted body fields or the user's `client_id` select the execution
principal. Any desired future user-level budget is additive and needs its own
policy; it must not replace the agent bucket accidentally.

Move route restriction extraction out of the single-principal helper. Preserve
`routeAlias` from the verified workload credential, and intersect it with any
applicable verified user restriction and existing control-plane alias binding.
Conflicting restrictions deny. A missing user `routeAlias` cannot erase the
workload restriction. A deployment that previously relied on a signed alias
claim must issue it in the replacement workload credential or have an explicitly
qualified equivalent binding; absence must not broaden access during migration.

For direct-user profiles, preserve existing identity and listing behavior. An
internal alias queried without an authorized workload remains indistinguishable
from an unknown alias, using the existing `AliasNotFound` behavior. Header
omission must not turn an internal alias public. Intentionally public aliases
retain their current policy; invalid supplied workload credentials still fail.

`agentDefault` selects a default and is not a separate authorization grant. Local
immutable agent model policy may further restrict selection but cannot expand
gateway access. Fallback deployments stay under the authorized alias policy.
Every request uses one snapshot generation; later tool-loop requests see newly
activated bindings. Immediate cancellation of admitted streams is outside scope.

## Errors and audit

Use `401` for invalid supplied credentials or credentials required by a route
profile independent of the requested model, `403` for authenticated callers
denied by endpoint or workload-context policy, and `503` for unavailable required
authorization state. For inaccessible internal aliases, including absent workload
identity on a direct-user profile, preserve `AliasNotFound` just as for unknown
aliases. Do not choose a missing-token `401` based on model lookup: that would
reveal that the model exists and requires an agent. Preserve the established public
error envelope and avoid disclosing restricted model existence. Detailed denial
reasons belong in internal audit and diagnostics with a correlation ID.

Audit admission denials as well as successful and failed inference completion.
Record:

- Correlation/request ID and available agent turn/session reference.
- Verified user identity and issuer; verified workload service identity and
  issuer; resolved agent definition and host/environment.
- Requested alias, effective model policy, selected deployment when available,
  and assignment/snapshot revision.
- Separate user-access and agent-assignment decisions and stable reason codes.
- Completion status, token usage, cost, and explicitly resolved billing subject.

Do not store raw tokens or unnecessary claims. A failed token check must not
label decoded claims as verified identity. A client-supplied turn identifier is
correlation metadata, not authorization evidence.

Apply the execution-identity table above to accounting and limits, and record
both actors independently. Regression tests must assert exact bucket and billing
keys, not merely that a request succeeds.

Extend the audit schema and WAL serialization together, with a compatibility
and migration strategy for persisted events. An observed local audit WAL
decoding failure must be diagnosed and repaired before live audit qualification;
it is a deployment observation, not proof of the cause of the original 403.
Honor each route's configured audit delivery guarantees and prove eventual
delivery for retained WAL records after recovery.

## Implementation phases

### Phase 1: Contracts and control-plane projection

The [Phase 1 contract](llm-dual-token-phase1.md) records the implemented typed
projection, compatibility checks, issuer constraints, and reauthentication
contract. Runtime activation remains gated on later phases.

Inventory existing delegation, canonical alias publication, and identity formats.
Type the existing `agentPolicy.gatewayDelegation` slot, identify actual projection
gaps, and specify issuer-supported token renewal and UI reconnect behavior.
Validate audience compatibility for both credentials and preserve execution,
route-binding, quota, and billing identity contracts. Add only demonstrated
schema/projection gaps with publication and digest compatibility tests.

Exit: existing alias bindings round-trip with the intended agent principal;
credential acquisition/renewal and user reauthentication contracts are concrete.
No new assignment store is introduced.

### Phase 2: Gateway verification and enforcement

Implemented: see [Phase 2 gateway implementation](llm-dual-token-phase2.md) for
the activation prerequisites, compatibility contract, and qualification gate.

Implement independent token validation, separate principal context, user rule
evaluation, assignment admission, and structured audit. Preserve unrelated
single-token admission profiles and provider credential isolation.

Exit: gateway integration tests prove allowed and denied cases before any
provider invocation, including real audit persistence.

### Phase 3: Agent forwarding

Implemented: see [Phase 3 Agent forwarding](llm-dual-token-phase3.md) for lifecycle
behavior, the repeatable gate, and remaining issuer/deployment prerequisites.

Add request-scoped dual-token support to the gateway model client and thread
the user context through interactive turns, retries, and tool loops. Give
noninteractive paths an explicit unsupported or separately authorized outcome.

Exit: concurrent requests cannot exchange credentials, the mock gateway sees
the correct pair, and reconnect and workload renewal pass expiry-boundary tests.

### Phase 4: Deployment and live qualification

See [Phase 4 rollout and qualification](llm-dual-token-phase4.md) for the issuer
change, executable checks, ordered rollout, rollback, and outstanding live evidence.

Deploy compatible gateway verification and projection support first, then agent
forwarding, then activate the required restricted-model policy. Prepare valid
service/user tokens and enforced expiry before activation. During transition,
any supplied second token is validated; no compatibility mode may ignore an
invalid credential or bypass an active assignment restriction.

Rollback must preserve enforcement: a gateway version that cannot enforce the
active policy must not serve it. Revert compatible policy and application
versions together through the normal publication/deployment process, or stop
affected traffic until enforcement is restored.

Exit: `/app/genai/chat` returns a model response for an authorized pair and
persists audit evidence identifying the user, agent, model, and policy revision.

## Verification matrix

| Test | Expected result |
| --- | --- |
| Valid user role, valid agent, matching assignment | Inference succeeds; both identities audited |
| Denied user role, valid assigned agent | 403; provider not called |
| Valid user, expired/invalid service token | 401; provider not called |
| Expired user, valid assigned agent | 401, including later tool-loop calls |
| Cross-host or wrong-environment agent | Denied before dispatch |
| Another agent's internal alias or removed binding | Same `AliasNotFound` as an unknown alias |
| Internal alias with omitted scope token on direct-user profile | Same `AliasNotFound` as unknown alias |
| Route profile requires both credentials, workload absent | 401 independent of model existence |
| Invalid supplied scope token on an unbound model | 401 |
| Intentionally unbound model with valid direct-user request | Existing policy preserved |
| Unknown or ambiguous workload mapping | Denied before dispatch |
| Assignment projection missing or invalid | 503; no unrestricted fallback |
| Binding revoked between two model requests | Next admission denies; revisions audited |
| User JWT has UI `client_id`, agent has bound principal | Binding, limiter, and billing keys remain the agent keys |
| User JWT lacks `routeAlias`, workload restricts alias | Workload restriction remains enforced |
| Conflicting signed alias restrictions | Deny before dispatch |
| Duplicate or combined credential headers | Reject before single-value access |
| Bearer scope header on new profile; raw scope on legacy profile | Correct independent parsing and compatibility |
| Tool/Knowledge `lad1` presented for LLM | Reject before replay consumption or dispatch |
| Static registry token lacks gateway audience | Profile cannot activate using that credential |
| User token lacks the gateway audience required by its issuer/profile contract | Profile cannot activate with that token contract; runtime rejects the token with 401 before dispatch, without disabling audience validation |
| Workload acquisition, expiry, refresh failure, and recovery | No stale-token loop; valid renewal resumes service |
| User expires on open socket, renews and reconnects | Same-owner session resumes without duplicate turns |
| Concurrent users sharing HTTP connection pool | Each request retains its own user token |
| Retry, tool continuation, and streaming admission | Same authorization contract enforced |
| Provider fallback or redirect | No policy escape or inbound credential disclosure |
| Audit sink outage and recovery | Configured delivery policy honored; retained events recover |

Use focused unit tests, a mock inference provider, and a real PostgreSQL audit
integration gate. Database tests skipped for lack of configuration do not count
as qualification. Complete the live UI-to-agent-to-gateway test with both
positive and negative assignment cases and persisted audit inspection.
