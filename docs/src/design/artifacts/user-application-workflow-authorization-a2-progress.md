# Authorization A2 Implementation Progress

Status: **the A2 source foundation and selected personal coding paths are
implemented, and the required base services pass local runtime qualification;
selected-stack A2 acceptance has not passed.** The separately published
workflow-only Agent services still fail Config Server lookup, so their A2
credential and job paths are not qualified. The A1 scheduled and hours-long
renewal tests remain deferred at the user's request. Nothing in this report
admits personal orchestration Phase 1.

## Implemented foundation

- `workflow-action` defines server-owned action bindings, canonical attempts,
  owner/boot/fencing identities, exact-byte request digests and child-lineage
  validation. Opaque references resolve stored permits; callers cannot supply
  replacement depth, class, grant or budget authority.
- The PostgreSQL ledger implements authorize, begin-dispatch, completion and
  status. Concurrent duplicates replay the same authorization; only a new
  `SEND_INTENT` acknowledgement permits initiation. Owner-bound `NOT_INITIATED`
  completion releases once and permits a new authorization generation. Historical
  completion receipts cannot release a newer reservation. `UNCERTAIN` cannot
  become retryable by lease expiry.
- Migration `0007_workflow_action_dispatch` adds authority, permit, dispatch and
  audit tables. It is present in canonical operational-store schema bundle
  `2.2.0`, with matching manifest/order/checksums. That schema-bundle version is
  independent of the container image release tag; the selected deployment uses
  image tag `2.3.5-dev.20260909.2338`. The local operational database has the
  schema and its canonical migration-ledger receipt.
- `light-security::dual_identity` checks separate user/app purposes, explicit
  issuer/audience/tenant, route-approved app service IDs and verified TLS peer
  fingerprints. Duplicate/coalesced credential headers are rejected. Workflow
  and receiver origins require an action reference. Missing verified peer context
  is rejected before any issuer/JWKS work. This helper does not replace route
  policy authorization.
- `light-axum::mtls` provides a certificate-verifying listener with bounded,
  concurrent handshakes and peer fingerprints from TLS, not forwarded headers.
- Workflow has a dedicated, optional mTLS action API and a fixed HTTPS control
  client in `light-client`. Authorize/begin/status verify user claims and hold
  broker grant/run read locks while checking the ledger. Completion authenticates
  the original service/peer without requiring an unexpired user token.
- `workflow.actionAuthorization` defaults to null. Enabling the listener requires
  the action migration, broker and strict JWT expiry verification. This switch
  alone does **not** establish a working A2 deployment.
- Pingora core 0.8.1 is vendored with an optional socket-level guard below TLS and
  buffering. Connection/TLS preparation precedes the guard; deadline check and
  first socket poll happen synchronously. Cancellation/expiry fences cleanup
  writes. Guarded HTTP/2 and unknown/custom transports fail closed.
- The proxy patch passes an optional guard to HTTP/1 and disables its default
  reused-connection retry for guarded requests. The new `guarded_http` adapter
  makes one connection attempt and one guarded send, with bounded response size
  and timeout, no redirects and no retry loop.

## Verification performed

These are source/foundation checks, not deployed-stack acceptance:

| Check | Result |
| --- | --- |
| `workflow-action` contract tests | 5 passed |
| Disposable PostgreSQL action-ledger test | 1 passed, executed against PostgreSQL |
| `light-security` unit tests | 19 passed |
| Real mTLS listener test | 1 passed |
| `light-workflow --lib` | 80 passed |
| `light-agent --lib` | 21 passed, 4 database tests ignored by their existing environment gates |
| `light-agent --bin light-agent` | 44 passed |
| `light-knowledge --lib` | 8 passed |
| Pingora core socket-guard tests | 5 passed |
| `light-pingora --lib` | 448 passed, 5 existing tests ignored |
| Guarded HTTP adapter tests, included above | 3 passed |
| Gateway action response classification | 2 passed |
| `light-gateway --lib` | 4 passed |
| `light-gateway --bin light-gateway` | 79 passed, 3 existing tests ignored |
| Combined Workflow/Agent/Knowledge/Gateway compile | Passed |
| Selected-package `cargo clippy --all-targets` | Passed; existing repository warnings remain |
| Operational bundle checksum validation | Passed |
| `git diff --check` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `mdbook build docs` | Passed; existing large search-index warning remains |

The PostgreSQL test uses its own disposable database, not the live operational or
credential stores. It covers simultaneous duplicate authorization/begin,
conflicting owners and changed bindings, opaque-reference mismatches,
non-initiation replay and reservation release, stale decisions, cancellation in
the foundation authority table, uncertain outcomes, child-run lineage, and
idempotent qualified-receiver reconciliation with conflicting evidence rejected.
The second pass extends this test to actual invocation cancellation and the
existing budget ledger, as described below.

Transport tests cover real plain connection reuse, TLS buffering after expiry,
pending writes, cancellation cleanup, and partial-send failure of a replayable
body without reconnection in the adapter. Full Gateway proxy-path first-write and
partial-write failure tests, with its actual retry configuration, remain pending.

Reproducible bounded checks are in
`scripts/run-authorization-a2-foundation-gates.sh`. The PostgreSQL test requires an
explicit empty disposable database URL; absence is reported as not run. No A1
soak is started by this script.

## Integration added in the second implementation pass

This pass adds real call-site wiring, but does **not** complete A2.

- Invocation admission accepts `renewableGrantId` only in the explicitly enabled
  A2 profile. The broker checks the exact consent binding, and the operational
  transaction installs run authority alongside invocation acceptance. Repeated
  broker run binding requires the same grant, user, tenant, work binding and
  expiry. Consent binding is `{profile: "workflow-action-v1",
  workflowDefinitionId, definitionDigest, policyDigest, responsePolicyDigest}`.
- The action ledger locks the actual `workflow_invocation_t` and
  `workflow_invocation_budget_t` records. Actual cancellation, deadline, policy,
  subject and budget-generation checks now apply. Action reservations charge the
  existing nested-call, byte and cost counters. Non-initiation refunds once;
  uncertainty retains the reservation. Initiated effects conservatively consume
  their configured byte/cost bounds until qualified actual-cost receipts exist.
- `bound_mcp::Runtime` is installed on the Workflow executor when A2 is enabled.
  It resolves the published dependency, renews through the broker, creates the
  action permit, and sends the original user credential plus the Workflow app
  credential over mTLS. Model-supplied authorization headers are not used on this
  path. The current producer supports root-run MCP tool actions only.
- Workflow's dedicated mTLS action listener also serves protected invocation
  routes. The ordinary listener rejects A2 invocation requests without verified
  peer context. Retrieval/cancellation/result routes receive the same strict
  user/app check. Existing route-level invocation authorization still applies.
- Gateway loads the restart-required `gateway.workflowActions` profile from
  `workflow-actions.yml`. Its TLS listener requests certificates from the
  configured client CA; ordinary browser connections may omit a certificate, but
  strict MCP routes require an approved verified peer. Origin is classified from
  verified credentials. `X-Workflow-Grant` selects a grant for root invocation;
  it is only a reference, and Workflow verifies the actual consent binding.
- The MCP request context now carries the verified A2 caller. Private targets
  inspect the stored action and require matching stable tool and contract
  metadata. Existing MCP policy and argument-mapping checks are retained.
  HTTP-tool and stateless MCP backend branches call the fixed action client and
  guarded transport. The destination registry checks the exact outbound URL.
  Platform credentials are stripped for destinations not configured for original
  user forwarding.
- Gateway boot registration is durable. The client generates a fresh boot ID;
  Workflow assigns a fencing generation. Repeated registrations replay, while a
  superseded boot cannot regain ownership. Authorize, begin and completion lock
  the current owner within their dispatch transaction. A new replica/boot can
  query latest state without acquiring send permission for an uncertain effect.
- Gateway does not treat HTTP `202 Accepted`, informational responses or
  redirects as terminal execution evidence. These responses retain an uncertain
  reservation and do not complete the Workflow step. A focused regression test
  covers these classifications. Target-receipt reconciliation remains pending.
- The socket guard now checks the deadline throughout request writes, including
  after a successful TLS control-record write. This conservative implementation
  also bounds request-body writing to the lease; response reads may take longer.

The PostgreSQL test now installs the real Workflow schema and constraints, seeds
an actual invocation/budget, and tests real cancellation, reservations and boot
replacement. It passed. Workflow's 80 tests, invocation-contract's 4 tests, and
proxy-framework's 448 tests passed (5 existing proxy tests ignored). The core
socket guard now has 5 passing tests. Combined Workflow/Gateway compilation
passed. These checks do not constitute an end-to-end A2 deployment test.

## Integration added in the third implementation pass

- Nested Workflow starts now carry `parentActionId`. Gateway derives depth,
  execution class and deadline from the verified parent binding, sends the child
  start through the guarded action transport, and includes the parent in its
  idempotency digest. Workflow proves the current Gateway owner and `SEND_INTENT`,
  verifies the child invocation fields, records parent action/run lineage, and
  inherits the exact renewable grant without copying refresh material.
- Workflow service Agent jobs are limited to bound coding/workspace inputs in the
  A2 profile. Separate interactive and workflow Agent instances use immutable
  service/definition/origin configuration, distinct credentials and verified
  mTLS listeners. Interactive instances do not consume the workflow job queue;
  workflow instances reject Chat, A2A, UI and upload ingress. Workflow checks the
  job's live invocation, grant, cancellation, deadline, depth and exact published
  Agent definition both before turn creation and before coding dispatch.
- Knowledge has an optional restart-required receiver profile and dedicated mTLS
  listener. Gateway forwards the current user token, its own scope token and the
  opaque action reference only to approved internal targets. Knowledge verifies
  Gateway user/app/peer identity, then uses its separate receiver identity to ask
  Workflow for the live action binding. Exact tool/contract/policy/disclosure
  mappings are checked before existing Knowledge user/resource ACLs run. When the
  profile is enabled, failure cannot fall back to the old `lad1` delegation.
- The frozen `complete` contract also accepts a qualified receiver receipt for
  an already `UNCERTAIN` generation. Workflow authenticates the receiver app and
  exact mTLS peer, checks its registered tool set, and accepts only terminal
  evidence for the same generation. Identical receipts replay; conflicting
  evidence fails; the retained reservation is settled once. Receivers cannot
  authorize or begin a dispatch through this path.
- Gateway terminal-response classification is target-specific. Informational,
  redirect and generic `202` responses are not completion evidence. A Workflow
  child `202` qualifies only when its bounded response contains the exact stable
  tool, a non-nil instance and a positive durable state version.

## Selected-stack runtime check on 2026-09-14

After rebuilding the selected local images and restoring the disposable
operational schemas, `./scripts/deploy-local.sh lt start` exited successfully and
reported that the required Compose services passed runtime qualification.
PostgreSQL, Config Server, Workflow, Gateway, Knowledge Admin, and the ordinary
interactive Agents were running; Gateway loaded its authorization policy and
registered with Controller.

This is not the A2 exit gate. The separately published
`com.networknt.agent.codex-personal-workflow-1.0.0` and
`com.networknt.agent.claude-personal-workflow-1.0.0` services received `404` from
Config Server for their `dev` snapshots and exited while parsing the incomplete
local fallback configuration. They were orphaned from the final qualified
Compose invocation and were not included in its required-service result. No
end-to-end authorize/begin/send/complete, renewal, revocation, receiver receipt,
or recovery matrix was completed in that run.

## Required work before A2 acceptance

1. Publish/import and activate the two workflow-only Agent Config Server
   snapshots, include those services in the selected A2 Compose topology, and
   qualify their distinct credentials and Workflow job paths. The required base
   services now start, but that does not qualify the A2 workflow-only identities.
2. Connect any asynchronous effect receiver that needs automatic recovery to the
   qualified `complete` receipt contract. Synchronous Knowledge responses and
   durable Workflow acceptance have immediate receipt policies; other unknown
   effects correctly remain non-retryable until their receiver supplies evidence.
3. Add and run the complete A2 integration/security/race matrix against the real
   receiving and sending paths. Existing regression suites predominantly exercise
   the default profile; they are not evidence that the new enabled profile works
   end to end. GitNexus reports critical aggregate change risk because Gateway and
   Workflow entry points, MCP dispatch, and Agent/Knowledge admission are all in
   scope. Do not remove legacy delegation before its replacement qualifies.

## Deployment state

The qualified-receiver completion contract is frozen. The local database contains
the A2 action migration and receipt. Portal contains separate `codex-wf` and
`claude-wf` definitions with service IDs
`com.networknt.agent.codex-personal-workflow-1.0.0` and
`com.networknt.agent.claude-personal-workflow-1.0.0`. Portal and runner-local
workspace bindings use authorization revision 3 and grant both interactive and
workflow-only identities. The local distribution has an enabled, ignored A2
runtime package containing marked app credentials, mTLS identities and exact peer
mappings; the installer contains the matching non-secret provisioning assets.
The rebuilt required base services pass local runtime qualification, but the
workflow-only services do not yet have retrievable Config Server snapshots and
their A2 paths remain unqualified. Existing unrelated working-tree changes are
preserved. No commit or push was performed.

The 2026-09-13 local inventory confirms that the published Codex and Claude
coding profiles and the owner workspace grant the existing
`com.networknt.agent.codex-personal-1.0.0` and
`com.networknt.agent.claude-personal-1.0.0` identities. Those running definitions
already have pre-A2 history and cannot be relabeled as workflow-only instances.
Those identities remain interactive. The separately published workflow-only
definitions preserve their history boundary; configuration aliases or a
caller-supplied origin header cannot cross it.
