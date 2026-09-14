# Authorization A0: Contracts And Issuer Profiles

Status: A0 contract baseline v1 frozen after review, September 13, 2026. The
qualified-receiver `complete` variant recorded below is part of the frozen A0
contract; it does not change the frozen operation set or authorize sends.
It defines the initial implementation contracts for the accepted
[authorization design](user-application-workflow-authorization.md).
It records source and local registration evidence, not completed A1–A3 runtime
qualification. Personal orchestration Phase 1 remains gated on those phases.

The baseline selects `portal-config-loc/all-in-lt` with local light-oauth for
the first unattended user profile. `light-portal-install` must publish the same
contracts when that distribution is qualified. External issuers and official
environments need their own completed profile manifest and qualification record;
neither inherits approval from a working local login.

## Baseline And Evidence

| Repository | Inspected HEAD |
| --- | --- |
| `light-fabric` | `3a13e20fd7a522d73a6d64a02e6e735ad681af73` |
| `portal-service` | `82895c28bcf41c9a0c65c54596d8cc5d8b20041c` |
| `portal-config-loc` | `161ae97e5d1b1ea0beaf6d3786f31f1220cdbda5` |
| `light-portal-install` | `50c6121e2dbc34f97a776df3edb69378def04427` |

These identify inspected source, not a claim that running images were built
from those commits. The running Compose project was confirmed as
`portal-config-loc/all-in-lt`, including its runner credentials override.
No credentials are included in this record or the accompanying inventory.

The [client inventory](artifacts/user-application-workflow-authorization-a0-clients.csv)
contains the 32 active provider/client bindings read from local
`configserver.auth_client_t` joined to `configserver.auth_provider_client_t` for
host `01964b05-552a-7c4b-9184-6857e7f3dc5f`. Both rows must be active. The export
includes IDs, names, types, profiles, exchange types, scopes and aggregate
versions; it excludes secrets, custom-claim values and user information.
Thirty-one bindings use provider `AZZRJE52eXu3t1hseacnGQ`; one uses
`AZ7MrzXYcz2X8kdd44FPLw`. Only the former is selected below.

For the selected provider, 30 clients are `trusted` and one is `confidential`.
`Light Portal Client` has exchange type `msal`; `pylon` has `ccac`; the other
bindings have no exchange type. These are observed registrations, not proof
that their traffic uses every grant available to them. In current light-oauth:

- `password`, `client_authenticated_user` and `long_lived` check
  `client_type == trusted`; `client_profile == service` does not disable them.
- Exchange is selected by `token_ex_type`. It is not a general offline grant.
- Refresh requires an authenticated client and a matching refresh record.
  The registry has no broker-specific strict-rotation or mTLS-authentication
  fields yet. A1 must add and enforce those restrictions.

## Selected Issuer Profiles

The following names identify this document's profiles, not existing config keys.

| Profile | Enrollment and credentials | Permitted use |
| --- | --- | --- |
| `local-workflow-user-v1` | Dedicated confidential broker client; user-present backend `authorization_code`, then `refresh_token`; issuer-verified `tls_client_auth` | Initial scheduled and long-running user workflow profile, enabled only after A1–A3 |
| `local-app-v1` | Existing issuer-approved `long_lived` app tokens plus registered mTLS workload identity | Immediate app identity in `X-Scope-Token`; never user enrollment authority |
| Existing interactive/specialized profiles | Current supported interactive grants, `client_authenticated_user`, client-credentials and MSAL/CCAC exchange | Preserve their separately approved uses; no automatic conversion to `local-workflow-user-v1` |
| External unattended profiles | None selected in v1 | Disabled until an issuer-specific renewable grant, identity mapping, freshness policy and recovery protocol are qualified |

The selected local provider uses issuer `urn:com:networknt:oauth2:v1`, shared
user audience `urn:com.networknt`, and provider ID `AZZRJE52eXu3t1hseacnGQ`.
The checked-in public issuer base is `https://oauth.localhost`; the provider's
discovery URL is not a substitute for the configured JWT issuer. Retain the
issuer's RS256 verification profile and separate long-lived app signing-key
purpose. Issuer-owned purpose/provenance must distinguish user tokens from app
tokens, including short-lived client-credentials tokens; `uid`, `sub`, token
lifetime or the header carrying a token is not sufficient evidence.

Freeze the local purpose marker as an issuer-signed `token_use` claim with
exact values `user` and `app`. A1 adds it to every local access-token issuance
path: user grants and their refreshes emit `user`; `client_credentials` and
`long_lived` emit `app`. Specialized user assertion/exchange grants retain their
supported user-token purpose, but that marker alone never proves eligibility
for Workflow enrollment; issuer grant provenance remains mandatory.

The issuer derives purpose from the validated grant/issuance path. Reserve
`token_use` against both registered custom claims and request extra claims, and
emit it as a dedicated `JwtClaims` field, separate from filtered `extra`.
`generate_jwt_with_key` calls `remove_reserved_claims(extra)` internally, so
inserting the reserved marker into `extra` would strip it before signing.
Pass the validated purpose explicitly into issuance and serialize it once as
`token_use`. Refresh must not derive it from a saved custom-claim snapshot.
A1 implements shared verifier
checks; A2 wires them at every selected receiver: user `Authorization` requires
`user`, and `X-Scope-Token` requires `app`. Unknown, malformed or conflicting
purpose is rejected. Markerless user or short-lived app tokens require new
issuance; they cannot fall back to `uid`, `role` or the shared provider key.
Existing markerless long-lived app fixtures may retain the explicitly registered
issuer/key-purpose app verification profile, only in the app position. A
conflicting marker is rejected even there; the normal shared signing key never
qualifies for this exception.

For `local-workflow-user-v1`, freeze these choices:

1. Create a dedicated broker registration. Allow only its backend code grant
   and refresh grant; reject `password`, `client_authenticated_user`,
   `client_credentials`, `long_lived` and exchange for this client. Do not
   repurpose either existing Workflow app registration from the inventory.
2. The issuer authenticates the user and records explicit workflow/schedule
   consent. Bind the code to the broker, provider, user, tenant, exact registered
   callback and enrollment intent. Use one-time state and PKCE S256, with the
   verifier retained by the broker. The browser supplies neither a refresh token
   nor a broker credential. A1 must implement the complete challenge path:
   current `post_code` stores `code_challenge` and `challenge_method` as `None`,
   even though redemption has a `verify_pkce` helper. Test code substitution,
   missing/wrong verifier, replay and consent/tenant mismatch. See
   [PKCE](https://www.rfc-editor.org/rfc/rfc7636.html#section-4) and
   [authorization-code protections](https://www.rfc-editor.org/rfc/rfc9700.html#section-2.1.1).
3. Use the dedicated issuer TLS listener for broker token, provenance lookup
   and revocation operations. Match the actual verified certificate to issuer
   registration; reject secret-only broker requests at every legacy URL too.
   This is [mTLS client authentication](https://www.rfc-editor.org/rfc/rfc8705.html#section-2.1),
   not automatic certificate binding of the forwarded user's access token.
4. Issue a bounded renewable family for this consent and backend client. Keep
   `auth_refresh_token_t`; apply live tenant-bound user/claim lookup on renewal,
   strict one-time rotation and no consumed-token successor recovery. A1 must
   expose issuer-owned provenance and authenticated family revocation. An
   existing refresh token with no eligible provenance cannot be enrolled.
5. Keep the initial user access-token lifetime at 600 seconds, matching current
   local issuance. Grant expiry and any shorter issuer limit cap renewal.
   An hours-long workflow does not obtain a longer-lived user access token.
6. Preserve the shared audience only inside explicitly approved forwarding
   destinations. Start with an empty destination allowlist until A2 publishes
   exact routes and receiver identities. Demo or third-party targets do not
   receive the platform user token merely because Gateway can reach them.

The broker's callback address, new client UUID, certificate identities, trust
roots and secret references are deployment bindings, not values to guess from
client names. A1 provisions them in the versioned manifest below. Enrollment
must fail while a required binding is absent.

## Claim Ownership And Freshness

| Claim or binding | Authority | Refresh/action rule |
| --- | --- | --- |
| Issuer/provider, user ID, tenant, broker client, consent, family and scope ceiling | Issuer grant/session plus accepted Workflow grant | Immutable binding; tenant cannot follow the user's currently selected Portal host |
| Account active/locked/verified and tenant membership | Current `user_t` and `user_host_t`, with applicable eligibility rules | Recheck on issuance/renewal; disabled or removed users cannot renew |
| `role` | Current `role_user_t` / `role_t` for the grant tenant | Rebuild eligible memberships; snapshot roles are not authority |
| `grp` | Current `group_user_t` / `group_t` for that tenant | Rebuild eligible memberships |
| `pos` | Current `employee_t`, `user_position_t` / `position_t` | Rebuild eligible positions in the grant tenant |
| `att` | Current `attribute_user_t` / `attribute_t` | Reload values used by policy |
| Custom policy-bearing claims | Explicit registered issuer/policy source per claim | Reload or fail; no fallback to a saved value |
| Custom application metadata | Client administration | Preserve supported metadata without allowing it to override user identity or permission authority |
| Run/action budget, depth, target and disclosure ceiling | Authoritative Workflow records | Resolve at authorize/begin; never take mutable values from copied JWT claims |

`portal-core::login_user_by_email` shows the relationship sources but selects
the current host and has login-specific filtering. `get_user_by_id` returns
NULL permission columns and also selects the current host. A1 needs a dedicated
authorization-context query; neither helper is the refresh contract unchanged.
Absent memberships mean an empty permission set, not permission to reuse the
old snapshot. Failed lookups issue no replacement token.

Freeze the initial freshness mode as **live refresh, existing access tokens
bounded by expiry**. Re-read applicable live policy/ACL data at each resource
decision. This does not discover a changed claim if that policy only consumes
the old JWT. The deployment manifest must record validator expiry leeway and
qualified issuer/receiver clock error; the worst-case old-claim interval is
the 600-second token lifetime plus those allowances. No immediate user-role
revocation claim is made. A stronger online user-status/version profile needs
separate qualification, not an undocumented cache assumption.

Workflow grant/run/action revocation has no positive authorization cache:
authorize and begin-dispatch read current primary-store state. A revocation
committed before begin denies a new send intent; a later revocation follows
the accepted in-flight cancellation rules. The local five-second initiation
deadline has no cross-host clock-skew allowance.

## Identity And Caller Contract

All new service records use `contractVersion: 1`, camelCase property names,
explicit tagged variants and rejection of unknown versions/fields. UUIDs are
opaque identities; counters are nonnegative integers with checked increment,
and overflow fails closed. Timestamps are UTC audit/expiry values. They never
reconstruct a Gateway monotonic lease after restart. An ID alone grants nothing.

`VerifiedIdentityContext` is constructed by trusted credential validation,
never deserialized as authority from public request headers:

| Field group | Required content |
| --- | --- |
| `user` | `issuerProfileId`, `issuer`, `providerId`, `subjectId`, `hostId`, `tokenClientId`, `tokenExpiresAt`, `claimsDigest` |
| `callerApp` | `issuer`, `providerId`, `clientId`, `serviceId`, `hostId`, `environment`, `registrationVersion`, `peerIdentity` |
| `origin` | `INTERACTIVE`, `WORKFLOW`, or `SYSTEM`, derived from the verified registration; system execution never fabricates a user |
| `actionBinding` | Required for every workflow-origin protected call: stored action reference, invocation and admitted actor/job binding |

For the `SYSTEM` variant, a verified service subject replaces `user`; it has
no impersonated user claims. That separate execution profile is not enabled by
selecting `local-workflow-user-v1`.

The local user subject is the verified `uid` mapped under the issuer profile;
`client_id`/`cid` identifies the login client, not the immediate workload.
The app `sid` is checked against registration, client and actual mTLS peer.
Conflicting identity claims fail instead of being merged. Existing Knowledge
subject/group/organization normalization and disclosure rules must be preserved
by the issuer mapping and verified policy context, not caller assertions.

| Receiver/operation | Allowed immediate caller for v1 | Additional checks |
| --- | --- | --- |
| Gateway business/model/API/MCP/Knowledge ingress | Explicit interactive clients, Workflow, or isolated workflow Agents | Verify user and caller independently; Workflow/workflow-Agent origin always needs an active action binding |
| Gateway-only business API/MCP/Knowledge receiver | Gateway registration and matching peer | Current user policy/resource ACL plus exact forwarding destination; direct Agent/Workflow/test-tool calls denied |
| Workflow action authorize/begin/status | Allowed Gateway and matching peer, directly | Current user and stored action/actor/tenant/attempt binding; not a general business-API bypass |
| Workflow action completion | Recorded Gateway owner, or an exact registered receiver/peer for qualified reconciliation | Can report evidence after user expiry; receiver evidence settles only the same uncertain action generation and cannot authorize a send or disclose results |
| Interactive Agent | Registered interactive ingress | Reject workflow jobs; do not expose interactive credentials to workflow execution |
| Workflow Agent | Trusted workflow admission bound to the selected job | Isolated registration, mTLS identity and secret mounts; cannot select interactive credentials |
| Issuer broker operations | Registered broker certificate/client | Backend consent/family/tenant binding; no secret-only fallback |

For the initial gateway-only profile, migrate Agent Knowledge calls through
Gateway with the same dual-token and disclosure checks. Do not leave the old
direct Agent-to-Knowledge `lad1` route as a workflow bypass. Any future direct
interactive Knowledge route needs an explicitly qualified separate profile.
Native personal workers remain consumers of admitted local work; they receive
no refresh credentials or authority to choose a caller profile.

## Grant, Action And Credential Records

These are new logical schemas. A1/A2 migrations and shared Rust types implement
them; this document does not claim the fields already exist in configuration.

| Record | Frozen fields and invariants | Owner |
| --- | --- | --- |
| `WorkflowGrant` | `grantId`, `generation`, `issuerProfileId`, `issuerGrantRef`, `subjectId`, `hostId`, `brokerClientId`, `consentRef`, `scopeCeiling`, `resourceCeilingRef`, `workflowOrScheduleBinding`, `notBefore`, `expiresAt`, `status`, `credentialRef`; references are tenant-bound and versioned | Workflow grant store; issuer provenance verified by broker |
| `WorkflowRunAuthorization` | `invocationId`, `grantId`, `grantGeneration`, `runGeneration`, accepted workflow version/digest, admitted actors, effective ceiling, `budgetLedgerId`, `budgetGeneration`, deadline and cancellation state | Workflow; recurring schedules create distinct run bindings |
| `WorkflowAction` | `actionId`, `invocationId`, `grantId`, actor/job binding, `parentActionId`, pinned logical operation/target/contract, `requestDigest`, policy/disclosure references, `permitDepth`, `executionClass`, reservation, deadline, generation and state | Workflow; untrusted tool arguments cannot allocate their own authority |
| `BrokerCredential` | `credentialRef`, grant/client/provider/tenant/family binding, encrypted refresh material, encryption-key reference, generation, renewal owner/fence, renewal-attempt identity and state | Dedicated broker store; no access from workflow DSL, runners or artifacts |
| `DispatchDecision` | `decisionId`, action/attempt/request/identity bindings, all grant/run/budget/owner generations, target/contract, depth/class/ceilings, reservation and lease | Workflow decision transaction; not a bearer credential |
| `DispatchReceipt` | Decision/attempt/generation, previous/new state, completing owner/boot/fence or qualified receiver identity, outcome/evidence reference, released reservation and audit time | `workflow_action_dispatch_t` and its retained transition evidence |

Grant admission requires `ACTIVE`; `REAUTHORIZATION_REQUIRED`, `REVOKED` and
`EXPIRED` forbid new protected actions. Reauthorization binds the same verified
user/tenant under a new generation without resetting budgets or uncertain
effects. One-off run completion retires that run binding; a recurring schedule's
parent grant survives only within its original consent and expiry.

The broker stores a durable renewal attempt before redeeming a refresh token.
Only the lease/fence owner may commit its replacement generation. A crash or
lost response after possible issuer commit fences the credential as uncertain,
requests family revocation and requires reauthorization. A database restore
must not resurrect an older refresh generation. Revocation retry cannot make
the fenced credential usable while the issuer is unavailable.

Encryption keys are outside ordinary database rows; ciphertext is separate from
ordinary workflow state. Access tokens may exist briefly in trusted memory.
Prompts, worker sessions, artifacts, logs, results and GitHub comments contain
no reusable credentials. The issuer keeps its own refresh/family history;
the Workflow broker store does not replace `auth_refresh_token_t`.

## Action Decision And Dispatch Contract

Freeze the four fixed internal POST operations from the accepted design:

| Path suffix under `/internal/workflow/actions/` | Request binding | Successful result |
| --- | --- | --- |
| `authorize` | Action/invocation/tenant, verified original caller, request/target binding, canonical `attemptId`, `gatewayOwnerId`, `gatewayBootId`, expected fencing generation | `decisionId`, exact bound decision and `AUTHORIZED` state; denial records evidence without an authorized row |
| `begin-dispatch` | Same attempt/decision/owner/boot and expected grant/run/budget/lease/fencing generations | First successful `AUTHORIZED -> SEND_INTENT` acknowledgement; remaining lease duration can only shorten Gateway's original deadline |
| `complete` | Decision/attempt/generation and either the recorded owner binding or an exact registered receiver/peer for an already `UNCERTAIN` action, plus tagged outcome and evidence reference | Durable receipt; identical duplicate completion returns the same receipt without another release; receiver evidence cannot begin or retry dispatch |
| `status` | Tenant/action/attempt and authenticated disclosure context | Current recorded state/evidence; never a new permission to send |

Workflow creates action records against admitted run/job and pinned dependency
records before protected dispatch. The executor and trusted Agent adapter pass
the resulting opaque action reference; it is not model-selected authority.
Each concrete protected operation has its own binding. Reserving a turn does
not authorize arbitrary later tool names, parameters or destinations.
The original caller in the internal request is an authenticated Gateway
assertion checked against that record, not a forwarded public identity header.

Required request/version/binding fields cannot be omitted or defaulted. A
duplicate begin reports existing state, **not** a fresh send acknowledgement.
A conflicting body or owner/generation fails. Lost responses recover through
the ledger; neither generic HTTP retries nor a status result authorizes replay.
`complete` distinguishes known terminal outcome, `UNCERTAIN` and owner-proven
`NOT_INITIATED`; business-result disclosure always needs current authorization.

For JSON request digests, reuse `workflow-invocation-contract`'s strict
`rfc8785-safe-json-v1` canonicalizer and `sha256:` lowercase-hex format. Keep
`requestDigest` separate from user-token bytes and from the logical input digest:
bind the pinned operation/target, actual method/path/query, effect-bearing
headers and payload. Credential headers are verified separately. Preserve
query/array order where meaningful. Binary payloads use a byte digest within
the canonical descriptor. A2 must verify the final request against this binding
after tool-to-HTTP mapping; changed targets, aliases or bodies cannot reuse it.
Unsupported/non-materializable request forms fail qualification rather than
omitting part of the operation from the digest.

Uniqueness is scoped to tenant/action/canonical attempt, with only one live
dispatch generation per action. Workflow locks current grant/run/action/budget
state in its operational PostgreSQL transaction. No lagging replica or positive
Gateway cache supplies approval. Lease expiry never deletes unresolved evidence.

| Recorded state | Permitted recovery |
| --- | --- |
| `AUTHORIZED` | Fence the old owner and seek fresh authorization for the same canonical attempt if no send intent exists |
| `SEND_INTENT` | Only the original first-transition acknowledgement permits its single guarded send; a lost acknowledgement or restart must not resend |
| `NOT_INITIATED` | Live original owner proves its guard aborted before start; release once, then reauthorize the same attempt under a new generation and current limits |
| `UNCERTAIN` | Reconcile through qualified target evidence/idempotency or an operator; no automatic effect replay |
| Known terminal | Reuse the authorized recorded result/evidence, not the operation |

The five-second timer starts before authorize and includes both control RPCs,
connection/TLS/protocol preparation and capacity waits. Only at the final
transport write boundary may the atomic deadline/acknowledgement check change
`NOT_STARTED` to `STARTED`. Expiry before it can close as
`ABORTED_NOT_INITIATED`; failure after it cannot claim non-initiation. Follow
the accepted design's owner/boot/fence checks, duration shortening and uncertain
recovery rules. No automatic request retry or redirect can introduce a second
send. Safe `NOT_INITIATED` retry retains incurred costs and retry counters.

Preserve existing `ExecutionClass` values `interactive`, `standard`, `batch`,
the checked `u16` depth ceiling and separate nonblocking synchronous pools per
depth. `StartInvocationRequest` needs verified parent action/decision binding
before replacing delegation-derived depth. Root classification is independent
admission; omission cannot reset a child to depth zero. Pin private-version
targets and their logical authorization name across alias changes.

## Migration And Transport Inventory

Paths below are source evidence relative to `light-fabric` unless prefixed
with another repository. Removal is gated on replacement behavior, not on a
text search for the word delegation becoming empty.

| Current path | A1–A3 disposition |
| --- | --- |
| `apps/light-workflow/src/executor.rs`: `DelegationSigner::mint` for nested calls | Replace copied user-claim `lad1` issuance with broker user credentials, Workflow app identity and online action decisions |
| `apps/light-gateway/src/main.rs`: `authenticate_agent_delegation`, workflow verifier and replay-store adapter | Replace with dual-token/peer verification and Workflow dispatch ledger; drain old effects before deleting replay authority |
| `frameworks/light-pingora/src/mcp.rs`: delegation context, private targets, depth/class and permits | Carry verified action context; preserve pinned target and nested-call protections |
| `apps/light-agent/src/main.rs`: `knowledge_authorization` and its upload/retrieve callers | Replace Agent-issued Knowledge tokens and direct route with the approved Gateway path; retain normalized subject and disclosure constraints |
| `apps/light-knowledge/src/lib.rs`: `authenticated_context` | Replace `DelegationVerifier` at retrieve, upload, MCP, document-version and passage routes; do not leave its legacy-acceptance window as a fallback |
| `crates/agent-delegation` and its Cargo dependents | Remove only after both Workflow/Gateway and Agent/Knowledge consumers migrate |
| `crates/workflow-invocation-contract/src/lib.rs`: `WorkflowDelegationClaims` | Migrate required depth, allowed-tool, tenant, deadline and budget checks; retain shared invocation/canonicalization contracts |
| `crates/agent-store/src/lib.rs`: replay-table inventory; operational SQL/schema bundles | Reconcile ownership/removal of `agent_delegation_replay_t`; keep new Workflow-owned dispatch evidence |
| `crates/llm-gateway/src/authorization.rs`, `crates/agent-runtime-protocol/src/gateway_delegation.rs` | Preserve dual-token `gatewayDelegation` policy and its explicit `lad1` rejection; the name does not mean it mints legacy tokens |
| `apps/light-workflow/src/invocation.rs` and `rule_api.rs` | Replace ordinary persisted bearer renewal with grant references; `load_status` token replacement is not unattended renewal |
| `workflow.invocation.ignoreUserJwtExpiry` | Must be false in the selected qualification profile, including dev; its current dev-only compatibility exception cannot qualify this design |
| Both distributions' Compose/startup/publication settings | Publish isolated identities, mTLS/trust, broker and dispatch settings; remove old signer secrets only after remaining consumers are drained |

The selected dependency baseline in `Cargo.lock` is Pingora/core **0.8.1**,
local `patches/pingora-proxy` **0.8.1**, reqwest **0.12.28**, and hyper-util
**0.1.20**. Hyper **0.14.32** and **1.9.0** both occur; trace the actual outbound
path instead of assuming either one owns every write. Dependency changes require
repeating transport qualification with the replacement lockfile.

| Outbound path | Concrete A2 qualification target |
| --- | --- |
| Pingora HTTP proxy | `patches/pingora-proxy/src/proxy_trait.rs::error_while_proxy` enables reused-connection retry; the loop in `src/lib.rs` consumes retry decisions. Disable these retries for protected dispatch |
| Pingora HTTP/1 and HTTP/2 | `proxy_h1.rs` and `proxy_h2.rs` call `write_request_header`; locate the actual first transport write below these calls. A handler hook or merely entering an async write is not proof that all queues are past |
| MCP HTTP/backend paths | `frameworks/light-pingora/src/mcp.rs` uses public/private reqwest clients and `ToolRetryPolicy`. Disable tool-level status/timeout/connect retry as well as lower-client retry for protected requests |
| Workflow-backed MCP control and child start | Bind the child start to the verified parent decision and retain existing idempotency. Do not confuse repeatable control/status operations with permission to resend a business effect |
| Model and Knowledge calls used by a workflow | Include their real client/proxy paths in the same inventory; no workflow-origin bypass through the interactive profile |

A2 must exercise new and reused connections, first/partial write failures,
delayed TLS/connection/capacity, replayable bodies, no hidden retries, and
monotonic deadline races on each qualified path. Count initiations as well as
target receipts. Until the required write boundary is controlled, that path is
not enabled for this profile.

## Deployment Manifest And Phase Handoff

A1/A2 must materialize one versioned, non-secret manifest for the selected
stack. Freeze its required content now:

- Issuer/profile/provider/tenant, allowed JWT algorithms, `token_use` enforcement
  and any explicit legacy long-lived app key-purpose mapping,
  broker registration/allowed grants, exact callback and issuer endpoints,
  certificate registration/trust/revocation settings and opaque secret references.
- Token lifetime, validator leeway, qualified clock-error bound and resulting
  maximum old-claim interval; current claim sources and custom-claim ownership.
- Exact Gateway, Workflow, interactive Agent and workflow-Agent client/service
  IDs, certificate peers, environment and allowed-caller/route mappings. Client
  names and Compose service names are inventory hints, not authentication.
- Exact approved forwarding destinations, pinned tool/endpoint/contract versions,
  resource/disclosure ceilings, supported transport paths and retry settings.
- Source revisions, built image digests, published configuration versions,
  migration versions, gate results and retained evidence references.

The current local startup defaults include
`com.networknt.portal.gateway-1.0.0`, `com.networknt.workflow-1.0.0`,
`com.networknt.agent.codex-personal-1.0.0` and
`com.networknt.agent.claude-personal-1.0.0`. They do not yet establish the new
workflow-only registrations. Gateway's startup environment defaults to `loc`
while Workflow/Agents default to `dev`; reconcile and validate the effective
published peer mappings during provisioning, not by weakening equality checks.
Neither existing Workflow client name proves it is the new broker.

Each isolated workflow Agent also needs its own service ID in
`codingProfile.workspaceBindings[].agents` and runner-local
`RunnerWorkspaceConfig.bindings[].agents`, with the complete bindings equal.
Keep intentional dev credential fixtures; official profiles must reject their
trust and use independent credentials.

| Handoff | Required deliverable |
| --- | --- |
| A0 to A1 | This v1 baseline, selected issuer/grant restrictions, claim ownership, current client/source inventory and the required deployment manifest fields |
| A1 | Issuer/broker implementation, issuer-owned `token_use` emission/reservation and shared verifier checks, enrollment including PKCE and consent/provenance, mTLS enforcement, live claims, strict rotation and failure evidence |
| A2 | Manifest-bound caller/destination policy, isolated Agents, action/dispatch stores and APIs, nested protections, and qualified transport behavior |
| A3 | Selected-stack end-to-end gates, old-authority migration/drain, compatible dev startup and independent official trust where applicable |
| Personal orchestration Phase 1 | Only after the selected A1–A3 qualification record passes; A0 by itself does not admit development workflows |

A1's purpose matrix must include a `client_credentials` token with registered
`uid`/`role` claims: it remains `token_use=app` and is rejected in user
`Authorization`. Test custom/request attempts to override purpose, every
issuance/refresh path, missing/unknown markers, user tokens in the app position,
and the narrowly scoped legacy long-lived app exception. A2 repeats receiver
integration tests. Inspect the actual signed JWT payload to prove exactly one
issuer-selected `token_use` survives filtering and serialization, including
custom-claim override attempts. A1 also records refresh attempts, uncertain
rotations and
reauthorizations during a multi-hour soak, separating ordinary-load observations
from injected response loss; see the parent design's A1 exit gate.

Changes to contract meaning, issuer grant eligibility or recovery transitions
require a new baseline revision and affected gate review. Reassigning environment
bindings also requires requalification of those bindings; it cannot silently
change origin classification, token purpose or lifecycle transitions.
