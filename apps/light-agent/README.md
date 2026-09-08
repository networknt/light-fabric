# light-agent

The enterprise agent service runs on `light-axum` and persists authenticated
sessions, turns, actions, approvals, and event projections in PostgreSQL.

Incoming requests, including `/chat`, accept the original user access token.
The configured `client.yml` JWK provider verifies its signature, and expiry is
enforced. Host and principal UUID validation remain mandatory. By default,
`security.issuer` and `security.audience` are empty: there is no additional
issuer/audience restriction unless operators configure one. This authentication
path does not require an incoming scope or an agent-specific `sid`, `serviceId`,
or `service_id` claim.

Consequently, a valid same-host token can authenticate to multiple agents that
trust the same JWK provider. Authentication does not establish a per-user grant
to a particular agent. A missing `agent_def_id`/`agentDefId` selects the deployed
Agent definition; an explicit mismatched or malformed definition is rejected.
Session resume still requires matching owner and Agent definition. These checks
are not a substitute for authorization of individual operations.

Gateway calls forward the caller's original access token unchanged. `tools/list`
and `tools/call` carry the same bearer credential the caller presented, which
light-gateway verifies as a normal JWT and evaluates through its access-control
rules. No token is minted on this path. `LIGHT_AGENT_ALLOW_BROAD_GATEWAY_TOKEN`
and `LIGHT_GATEWAY_AGENT_DELEGATION_SECRET` have been removed; remove them from
deployments.

`LIGHT_AGENT_DELEGATION_SECRET` with at least 32 bytes is still required, because
Knowledge access continues to mint a delegation. Removing that last minting path
is tracked in light-fabric#373.

Authenticated upload clients obtain a dedicated 60-second Knowledge delegation
with `POST /knowledge/upload-delegation`. The returned token is valid only for
`light-knowledge`, carries the server-owned host, Agent, policy, and environment
binding, and cannot be used for retrieval. `light-knowledge` still authorizes
the requested Knowledge Base and active `UPLOAD` source before accepting bytes.
Those checks authorize the Agent's KB binding and source, not the individual
caller's upload permission. The minting endpoint currently has no separate
caller-level upload authorization check; accepting a user token must not be
interpreted as proof that this additional permission was evaluated.

Memory writes use the embedded Memory API/repository and remain operational
state in `operations.agent_ops`. Portal-command and direct-Config-Server write
modes are rejected after the Phase 4 cutover. Portal publishes memory policy
and hard directives; it is not on the conversation write path.

Apply `portal-db/postgres/patch_20260711_01_light_agent_runtime.sql` after the
workflow-runner migration before starting this version.

Light Portal publishes one immutable Agent audience projection to Config Server
for each Agent runtime instance. `agent.yml` is the typed template and the
current snapshot's `values.yml` supplies its complete `operationalStore`,
`runtimePolicy`, and `agentPolicy` namespaces. Startup reads the database URL
only from the deployment-owned `0400` file, validates the exact
`operations_agent_runtime` role and `agent_ops` authority, then validates the service/environment binding,
validity window, policy/content digests, Agent definition/version, policy snapshot,
model Alias, tools, skills, execution limits, catalog, memory, Knowledge
bindings, channel, data-boundary, and session policy before accepting work.
The startup Agent-definition pointer, model binding, effective catalog, and
Knowledge bindings are no longer resolved from Portal database tables or
Portal Query calls.

The model provider is always `gateway`. The published model value is an Alias
understood by `llm-gateway`; provider endpoints, credentials, and provider
specific configuration are not accepted by Light-Agent. The former
`model-provider.yml` and provider-specific YAML files have been removed.
The existing workload bearer from `LIGHT_PORTAL_AUTHORIZATION` authenticates
the call to `llm-gateway`; it remains secret bootstrap material and is not
published inside the immutable Agent projection. Portal controls which Alias
that workload may use through the corresponding gateway audience policy.

`operations.agent_ops` is the durable operational store for sessions, turns, actions,
approvals, immutable pinned-policy copies, quota counters, and audit evidence.
Light-Agent has no Config Server database credential.
Resuming a session requires the exact policy snapshot and Agent-definition
version admitted from Config Server.

Session expiry and projection maintenance run independently of execution polling.
Each reconciliation step backs off separately on failure (2 seconds up to a
60-second cap) and logs its stage and next retry interval. A failed execution
service therefore cannot indefinitely prevent expired sessions from releasing
active-session slots. Database failures can still delay local cleanup.

If WebSocket session initialization fails, the agent sends an `error` frame with
a machine-readable `code`, then a close frame. `SESSION_LIMIT_EXCEEDED` uses
close code 1013; other initialization failures use 1011 with a safe public
message. Full internal errors remain in agent logs.

Shared runner state is owned by Controller in `operations.execution_ops`, not
by the Agent database connection. Portal publishes
`agentPolicy.execution.executionApiUrl`; the existing workload token must
carry the Agent service ID, exact Host, and `execution.invoke`. Agent commits
request and cleanup commands to `agent_execution_outbox_t` in the same local
transaction as its turn/session state, retries that handoff idempotently, and
acknowledges a terminal Controller result only after its local result
transaction commits. The outbox moves with Agent state in database-topology
Phase 4; it is never execution authority.

The process-wide `AgentState` contains only shared infrastructure and caches.
Provider, model, definition version, policy, data boundary, product profile,
service pool, compatibility digest, and effective catalog identity are resolved
again from the activated durable turn. The signed caller may select an agent
definition with the `agentDefId`/`agent_def_id` claim; deployments without that
claim retain the configured single-definition fallback. A definition update,
policy revocation, or pool-assignment revocation therefore cannot silently
change an already admitted turn or reuse a mismatched catalog entry.

Queued turns are dispatched across sessions under a host-scoped PostgreSQL
advisory transaction lock. Dispatch first favors principals with fewer running
turns, then the principal least recently activated, then durable FIFO creation
order. The same transaction enforces session exclusivity and pool concurrency;
all replicas use this path, so scale-out does not create a per-process fairness
island.

Waiting WebSockets do not poll PostgreSQL. Each replica owns one dedicated
`LISTEN` connection for queue, capacity, and activation notifications plus an
in-memory per-turn `Notify` registry. Admission and terminalization publish
transactional notifications; any replica may perform the serialized dispatch,
and the activation notification wakes the replica holding the corresponding
WebSocket. The listener subscribes before its catch-up pass and runs a bounded
five-second catch-up after reconnect, so notifications are latency hints rather
than the source of truth.

## Profile dispatch

Messages without a `profile` retain the enterprise gateway/model behavior.
Coding and personal edge execution use closed, typed payloads and are dispatched
only after the durable turn acquires its session fence and its published
`productProfileDigest` matches the operator-enabled profile.

Light Portal enables the Codex coding profile by publishing all of the following
`agentPolicy.execution.codingProfile` values; partial configuration fails
startup:

```yaml
agentPolicy:
  execution:
    codingProfile:
      schemaVersion: 1
      productProfileDigest: sha256:<published-coding-profile-digest>
      repositoryUriPrefix: file:///var/lib/light-agent/repositories/
      adapterId: codex-app-server-v1
      adapterVersion: 0.153.4
      adapterProtocolVersion: codex-app-server-v2
      actionKind: coding.codex-app-server-v1
      compatibilityDigest: sha256:<approved-cube-compatibility>
      imageDigest: sha256:<approved-runner-image>
      capabilityDigest: sha256:<approved-runtime-capabilities>
      templateId: coding-codex-app-server-v1
      templateVersion: 1
      templateDigest: sha256:<approved-command-template>
      executable: /usr/local/bin/codex
      binaryDigest: sha256:56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da
      schemaDigest: sha256:d3eace08be5dca386bfd1f1e8df650058b4113f1e10870a284d775d75517576a
      requiredFeatures:
        - restricted-model-egress
        - immutable-repository-upload
        - canonical-patch-output
        - codex-app-server-v1
      qualification:
        schemaVersion: 1
        adapterId: codex-app-server-v1
        adapterVersion: 0.153.4
        status: qualified
        evaluatedDimensions: [protocol-lifecycle, approval-mediation, streaming-events, usage-accounting, cancellation, resumability, canonical-patch, review-isolation, authentication-profiles, workspace-isolation, panic-containment, dependency-compatibility, license-compatibility]
        contractDigest: sha256:<exact-launch-contract-digest>
        evidenceDigest: sha256:6fe22317953bbfd2192ae9c4bca64828b447731ee00940041b1395f5f7b50bf4
      model: coding-implementer
      reviewModel: coding-reviewer
      # personal-subscription or enterprise-api. This value is immutable for
      # every admitted coding turn.
      authenticationProfile: enterprise-api
      # Omit for the local personal-subscription profile. In enterprise mode
      # every value is trusted policy; the repository cannot override it.
      enterpriseGateway:
        baseUrl: https://llm-gateway.example/v1
        credentialTarget: llm-gateway-attempt
        audience: llm-gateway
        routeDigest: sha256:<runner-broker-route-digest>
        budgetPolicyId: developer-default
        maximumRequests: 1
        maximumTokens: 200000
        maximumCostMicros: 5000000
        maximumResponseBytes: 16384
```

The corresponding runner credential directory contains one owner-only JSON
envelope named `<attempt-binding-sha256>.json`, with `schemaVersion`,
`credentialId`, `generation`, `token`, `audience`, `bindingDigest`, `issuedAt`,
`expiresAt`, and optional `revokedAt`. The token broker must mint it for that
exact coding attempt and expire it within five minutes. It re-reads the envelope
when delivering a credential, so a higher generation replaces a not-yet-issued
credential and a revoked envelope fails closed. Per-attempt names avoid a shared
credential race when turns run concurrently. The runner gives it only to the
Codex App Server process; generated shell and tool processes explicitly exclude
`LIGHT_LLM_ATTEMPT_TOKEN`.

The repository source adapter must place immutable Git bundles under the
configured spool before the message is admitted. Arbitrary local paths and
remote URLs are rejected. Light-Agent constructs the materialization manifest
itself and currently admits no client-selected skill packages.
The pinned runner image reports the exact capability document and digest with
`light-agent-worker print-capabilities`; publish that value rather than a
hand-authored digest.

```json
{
  "clientMessageId": "01-coding-turn",
  "profile": "coding",
  "text": "Update the parser and its tests.",
  "coding": {
    "repository": {
      "artifactUri": "file:///var/lib/light-agent/repositories/acme/repo.bundle",
      "digest": "sha256:<bundle-digest>",
      "size": 12345,
      "mediaType": "application/x-git-bundle"
    },
    "baseRevision": "<40-or-64-hex-commit>",
    "workspaceRoot": "/workspace/repo",
    "writableRoots": ["/workspace/repo"],
    "allowedTools": ["fs.read", "fs.write", "process.exec"],
    "maximumPatchBytes": 131072,
    "maximumChangedFiles": 100
  }
}
```

Personal edge actions require Light Portal to publish
`agentPolicy.memory.personalProfileDigest`. The typed `edgeAction` contains
`edgeBindingId`, `action`, `arguments`,
`schemaDigest`, and an optional `approvalId`. The server revalidates the live
principal-bound edge runner projection, exact action schema, effect class, approval, runner
identity, backend identity, and compatibility digest before enqueueing. Direct
edge turns terminate from the accepted runner result; they do not leave a
session waiting for a nonexistent in-process model continuation.

## Trusted quota accounting

Token and cost quotas reserve the admitted turn ceiling transactionally.
Enterprise model usage is accepted only from the server-owned provider adapter
and cost is calculated from the immutable projected model rate pinned on the
turn. Runner-backed model usage is accepted only from
runner-journal broker counters copied into terminal evidence; sandbox-provided
`usage` fields are ignored. Missing or uncertain usage settles at the reserved
ceiling instead of refunding capacity. Pre-dispatch failures explicitly release
their reservation. Every reconciliation records its accounting source and, for
trusted actuals, an evidence digest, making retries idempotent and auditable.

Deployments with cost quotas must publish an enabled model rate for the exact
host, provider, and model before admitting a turn. Rates are expressed as
micro-units per one million input or output tokens and are snapshotted at
admission so later rate changes cannot alter an in-flight turn.

The immutable projection already reserves
`agentPolicy.execution.quotaPolicies`, `modelRates`, `servicePools`,
`edgeRunnerBindings`, and `approvalRules`. Quota policies, rate cards,
edge-runner bindings, and service-pool definitions are typed,
validated inputs from `agent.yml`. Admission pins their versions and digests;
PostgreSQL retains only accounting windows, reservations, sessions, turns,
pool occupancy, and reconciliation evidence. Dispatch locks operational
session/turn rows and never reads or locks Agent control-plane authoring
tables.

Runtime Config Server/controller activation is also intentionally not enabled
by this startup slice. A safe reloader must validate the complete candidate,
atomically switch admission to it, retain older snapshots for pinned sessions
and turns, and keep the last-known-good snapshot on rejection.

### Portal chat capabilities

Portal chat selects active deployed instances filtered by product `agt` in the
current Host. It uses the instance service ID and environment tag for `/chat`
routing. The Gateway terminates the browser's `csrf.<token>` WebSocket
subprotocol and forwards the verified bearer identity to the Agent.

After session admission, the Agent sends `type: "session"` with `session_id`,
`turnTypes`, and `defaultTurnType`. Ordinary agents advertise `["chat"]`;
agents whose configured coding profile matches the admitted session policy
advertise `["chat", "coding"]`. Chat remains the default. These fields describe UI capabilities, not permission
to execute: durable turn admission and coding policy checks still apply.
Older clients ignore the additive fields. Updated Portal clients fall back to
ordinary chat when talking to older agents; deploy the updated Agent to expose
coding selection and the updated Gateway for browser CSRF protocol negotiation.

Browser WebSocket routes require an explicit Origin allowlist. Configure the
Portal's exact scheme and authority in Gateway `values.yml` (replace the example
origin with the deployment's Portal origin):

```yaml
websocket-router.originAllowlist:
  /chat:
    - https://portal.example.com
```

The `/chat` entry also covers `/chat/` and its descendants. Other browser routes
need their own allowlist entry; query/header service selection does not bypass
this check. Missing, opaque (`null`), malformed, and unlisted origins are denied.
Keep any existing `/ctrl/mcp` allowlist entry when adding `/chat`.
