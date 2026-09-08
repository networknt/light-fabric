# Agent LLM dual-token Phase 2 implementation

Phase 2 implements gateway admission and durable identity audit.
[Phase 3](llm-dual-token-phase3.md) now supplies Agent forwarding, credential
renewal, and interactive reauthentication. Deployment activation remains Phase 4.

## Admission and identity

Trusted `llm-router.agentDelegation.endpoints` selects the dual-token parser for
POST chat completions, Responses, and Anthropic messages. A `true` value requires
both tokens; `false` permits user-only calls and validates any supplied workload
token. Model listing and routes outside this map retain their existing profile.
An overlapping HMAC profile fails closed. There is no parser retry or legacy
`lad1` fallback on these routes.

`Authorization: Bearer <user JWT>` and `X-Scope-Token: Bearer <workload JWT>`
are parsed from all header values; duplicates and malformed forms are rejected.
Both signatures use the shared JWT verifier. Issuer, audience, expiry, and
not-before checks are enforced independently of legacy expiry bypass settings.
The workload must match a current projected registration, host, environment,
scopes, and route alias. Expired publication evidence fails closed.

User access-control rules receive the user principal. Model binding and limiter
identity receive the registered agent definition ID; billing uses the verified
workload billing subject with the agent ID as fallback. Signed route-alias
restrictions intersect the published alias. Existing internal aliases and
`boundPrincipal` remain the model assignment authority. Provider requests strip
both incoming credential headers and use provider credentials.

## Publication and activation prerequisites

The Portal candidate compiler materializes `llm-router.agentDelegation` from the
existing published `agentPolicy.gatewayDelegation`, current Agent snapshots,
active agent definitions, and active OAuth registrations for the same host and
environment. It carries registration version, policy digest, and publication
expiry. This is a derived projection, not another assignment store; no client
secret or secret-file reference is exported.

Register the optional `agentDelegation` property in Portal's `llm-router` config
metadata before publishing the gateway candidate. Older metadata remains
compatible. Initial generated inference endpoints permit direct user calls;
subsequent publication preserves trusted endpoint requirements. Removing the
last binding retains the profile with an empty binding list instead of restoring
legacy parsing. Registration revocation takes effect after gateway publication
and reload, bounded by the projected publication expiry.

Before activation, apply audit PostgreSQL migrations through
`0006_authorization_context.sql`, configure the audit database environment
reference and writable WAL, and require `local_durable` audit for generation
aliases. Binding host IDs must match the audit host. Gateway loading rejects
missing aliases and conflicting internal alias bindings. Prepare issuers that
actually issue the specified audiences; audience checking must remain enabled.

Requests pin both the verified identity and the matching routing snapshot across
reload. Runtime snapshots share concurrency permits so reload cannot reset limits.

## Audit and compatibility

Admission denials and authorized inference records persist structured
`authorizationContext`, including verified user/workload identities, resolved
agent and registration evidence, correlation ID, and access/assignment decisions.
Unverified claim values and bearer credentials are not recorded. Audit admission
failure returns 503 before provider dispatch. Missing or inaccessible internal
models retain the existing indistinguishable 404 response.

The JSON field is additive and absent for legacy calls. PostgreSQL migrations
are idempotent. Deploy the updated audit consumer before activating the profile;
do not replay a WAL containing dual-token records through an older consumer,
which does not preserve the added identity field. Drain the WAL with the updated
consumer before rolling back. Existing legacy admission profiles remain unchanged.

## Verification

Run `scripts/run-agent-llm-phase2-gates.sh` with
`LLM_AUDIT_TEST_DATABASE_URL` pointing to a disposable, dedicated PostgreSQL
database. The gate refuses to silently skip database qualification.

The gate checks Java publication contracts against the Rust fixture, applies
migrations twice, runs gateway unit/data-plane/alias regressions, explicitly runs
the PostgreSQL idempotency test, and starts an actual Pingora gateway with mock
JWKS and provider servers. The live test verifies successful dispatch, user RBAC,
audience/expiry checks, host/scope/alias/registration failures, missing or duplicate
credentials, legacy token rejection, provider credential isolation, and persisted
user/workload/agent attribution. Java publication tests use mocked JDBC; the audit
sink test uses real PostgreSQL. This is gateway qualification, not a live Portal
chat or credential-renewal qualification.
