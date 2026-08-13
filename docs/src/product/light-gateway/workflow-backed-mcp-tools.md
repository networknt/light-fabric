# Workflow-Backed MCP Tools

Status: Development implementation; runtime qualification incomplete

This repository is still in development. The workflow-backed MCP path has not
been exercised against a live workflow deployment, so none of the phase gate
scripts or unit-test results constitute production qualification. The current
implementation intentionally targets one clean contract; migration adapters,
legacy binding formats, and backward-compatibility rollout procedures are out
of scope until the runtime behavior is proven.

## Implementation And Qualification Status

| Area | Implementation | Qualification |
|------|----------------|---------------|
| Phase 0 contracts, canonicalization, and threat-model fixtures | Implemented | Unit/fixture and disposable PostgreSQL contract checks pass; latency evidence not run. |
| Phase 1 synchronous gateway/workflow path | Implemented | Rust tests pass; no deployed end-to-end workflow or concurrency/fairness evidence. |
| Phase 2 asynchronous, effect, cancellation, and compensation path | Implemented | Component tests pass; no live side-effect or recovery exercise. |
| Phase 3 AI-assisted draft authoring | Implemented | Java/UI checks pass; no production model or reviewer workflow qualification. |
| Phase 4 optional skill binding | Implemented | Component and disposable PostgreSQL constraint checks pass; no live agent/catalog exercise. |
| Runtime promotion | Disabled | Remains disabled until the numeric Phase 0/1 qualification evidence is recorded. |

Requirements are indexed by their owning sections: binding and runtime fields
under **Tool Contract** and **Invocation Contract**; error classes under
**Failure Mapping**; security controls under **Authorization**, **Delegation**,
and **Destination Safety**; and verification requirements under each phase's
**Exit gates**.

This document defines how `light-gateway` should expose an orchestration as an
ordinary MCP tool while `light-workflow` owns the durable multi-step execution.
The design lets existing MCP-capable agents consume a higher-level business
capability without adding Light-Portal skill support or reproducing API
sequencing inside the agent.

The core principle is:

> The gateway exposes and governs the tool contract; the workflow runtime
> executes the orchestration.

## Problem

The MCP router can currently expose a backend MCP operation or translate an MCP
`tools/call` into one backend API request. That works well when a backend
endpoint is already meaningful to an agent.

Many enterprise APIs are more granular. A useful business operation may need
to:

1. load data from several APIs;
2. transform and join their responses;
3. apply business rules or conditional branches;
4. call another API with the derived input;
5. normalize transport-specific responses into one stable result;
6. retry transient failures or compensate for partial side effects; and
7. pause for approval or continue asynchronously.

Making the agent issue each low-level call is inefficient and exposes internal
API structure to every agent implementation. A Portal skill can guide an agent
through the process, but existing customer agents may understand MCP without
understanding Light-Portal skills or skill-to-workflow bindings.

The same high-level capability therefore needs to be available directly from
the gateway's MCP `tools/list` and `tools/call` surface.

## Goals

- Expose an orchestration as a normal, schema-bound MCP tool.
- Avoid requiring existing agents to adopt Portal skills.
- Keep one canonical executable workflow definition.
- Keep authorization, input validation, response filtering, and output
  validation at the gateway boundary.
- Keep sequencing, branching, transformation, retries, durable state, human
  tasks, and compensation in `light-workflow`.
- Support both bounded synchronous tools and explicit asynchronous tools.
- Let users author the workflow manually or ask AI to generate a reviewable
  draft in `portal-view`.
- Publish immutable workflow and schema references to the gateway through the
  existing configuration control plane.
- Preserve tenant isolation, stable tool identity, audit correlation, and
  least-privilege delegation for every nested call.

## Non-Goals

- Do not embed a general-purpose workflow engine in `light-gateway`.
- Do not execute user-authored JavaScript, Python, or shell code in the gateway
  process.
- Do not let model-generated text choose arbitrary backend URLs, credentials,
  service IDs, or workflow definitions at runtime.
- Do not make every agent turn a workflow.
- Do not require a skill assignment before a workflow-backed MCP tool can be
  invoked.
- Do not copy the workflow DSL into every gateway instance as the executable
  source of truth.
- Do not make the gateway read Portal or workflow database tables directly.

## Decision

Introduce a workflow-backed MCP tool as a third gateway execution type:

```text
Existing MCP agent
  -> light-gateway tools/list
  -> light-gateway tools/call
  -> gateway authorization and input validation
  -> workflow invocation API
  -> light-workflow durable execution
  -> gateway response filtering and output validation
  -> MCP tool result
```

The tool looks like any other MCP tool to the caller. Its implementation is a
versioned workflow reference rather than one HTTP endpoint or downstream MCP
operation.

The gateway contains a small workflow dispatch adapter. It does not interpret
workflow steps, maintain workflow state, run compensations, or execute
transform expressions.

## Why Not Orchestrate Inside The Gateway

Gateway-native orchestration may appear to reduce one network hop, but it would
create a second orchestration runtime in the data plane. That runtime would
need independent solutions for:

- durable state across gateway restarts;
- idempotency and duplicate requests;
- retries, backoff, and per-step deadlines;
- fan-out, joins, and partial failures;
- compensation after side effects;
- human approval and long waits;
- workflow version snapshots;
- cancellation and abandoned callers;
- cycle detection and bounded recursion; and
- per-step audit, metrics, and traces.

Those are workflow-runtime concerns. Keeping them in `light-workflow` also
prevents a slow or waiting business process from consuming gateway execution
state.

A separate composition service should be considered only if production
measurements prove that the durable workflow path cannot meet a required
interactive latency target. If introduced, it should execute the same compiled
and governed workflow representation instead of creating another authoring
language.

## Relationship To Skills

A skill and a workflow-backed tool serve different purposes:

| Object | Responsibility |
|--------|----------------|
| Skill | Agent-facing guidance, examples, discovery hints, and optional progressive disclosure. |
| MCP tool | Stable executable input/output contract visible through `tools/list`. |
| Workflow | Canonical orchestration, transformations, branches, and durable execution. |

The existing [Skill Workflow Orchestration](../../design/skill-workflow-orchestration.md)
design links a skill to a canonical workflow through `skill_workflow_t`. This
design adds a second, independent exposure for the same workflow:

```text
workflow definition
  |-- optional skill_workflow_t link for skill-aware agents
  `-- workflow tool binding for all MCP-capable agents
```

An agent that supports Portal skills can receive richer instructions and
examples. An existing agent can discover and call the composite MCP tool with
no skill integration. An agent with a static tool allowlist must add the new
tool name or refresh that allowlist, but it does not need a new orchestration
framework.

## Current Foundation And Gaps

The existing platform already provides most control-plane and runtime pieces:

- `tool_t` has a stable tool reference, model alias, schema digest, dispatch
  policy reference, and `execution_placement = workflow`.
- `wf_definition_t` stores the canonical versioned workflow YAML.
- `skill_workflow_t` can optionally connect a skill to a workflow.
- `portal-view` has a workflow YAML editor, outline/graph support, client and
  server validation, test input, workflow start, and runtime-state inspection.
- `light-workflow` snapshots the definition digest and resolved execution
  policy when it consumes `WorkflowStartedEvent`.
- `light-workflow` can execute HTTP and MCP calls and can use `set`, `switch`,
  `assert`, and output exports for sequential compositions.
- `light-gateway` already performs MCP tool authorization, input-schema
  validation, backend dispatch, response filtering, output-schema validation,
  resource limits, and audit logging.

The integration is not turnkey yet:

- the gateway runtime accepts only HTTP/OpenAPI and MCP tool execution types;
- workflow start currently enters through `workflow-command`, and runtime
  status is read from workflow query projections;
- `light-workflow` does not yet expose a stable start/wait/status/result/cancel
  service boundary for gateway use;
- the current runtime expression evaluator supports only a limited path,
  interpolation, literal, and comparison subset rather than the production CEL
  expression contract defined below;
- each `light-workflow` service instance currently runs one serial host-task
  executor over a global cross-tenant claim ordered by priority and age, sleeps
  for 500 ms after an empty claim, and reclaims stale Boolean locks after five
  minutes without a fencing token; and
- generic fork/join and retry behavior must be completed before advertising
  broad production orchestration semantics.

These gaps should be closed in the workflow and gateway runtimes rather than
worked around with executable logic in gateway configuration.

## Component Responsibilities

### Light Portal

The Portal control plane owns:

- workflow and composite-tool authoring;
- stable identities and versions;
- schema extraction and validation;
- workflow-to-tool bindings;
- dependency resolution and cycle checks;
- policy and safety metadata;
- test fixtures and promotion evidence;
- review, approval, publication, rollback, and retirement; and
- projection of runtime-ready `mcp-router.tools` configuration.

The control plane must normalize flexible UI input into the strict gateway
runtime contract before persistence. The gateway must not infer missing
workflow identity, safety policy, or schema bindings from model input.

### Portal View

`portal-view` owns the manual and AI-assisted authoring experience. It does not
execute production workflows or publish AI output without validation and
approval.

### Light Gateway

The MCP router owns:

- `tools/list` exposure and tools-list access control;
- tool name to stable workflow-tool binding resolution;
- caller authentication and composite-tool authorization;
- input-schema validation and argument masking;
- bounded workflow dispatch;
- correlation, delegation, and idempotency context;
- concurrency, payload, and deadline limits;
- mapping workflow terminal state to an MCP result;
- response filtering and output-schema validation; and
- gateway-level audit and diagnostics.

The gateway does not parse or execute workflow tasks.

### Light Workflow

The workflow service owns:

- validating the requested workflow reference and expected digest;
- durable instance creation and idempotent start;
- definition and execution-policy snapshots;
- task sequencing, branching, transformations, retries, and joins;
- HTTP, MCP, rule, agent, human, and runner task execution according to policy;
- workflow deadlines, cancellation, and compensation;
- public-result construction and output-schema validation;
- instance, task, event, and audit state; and
- start, wait, status, result, and cancel APIs.

## Control-Plane Data Model

Keep the workflow YAML canonical in `wf_definition_t.definition`. Use
`tool_t` for the agent-facing tool identity and set:

```text
stable_tool_ref       = immutable logical tool identity
execution_placement   = workflow
model_alias           = gateway-facing MCP tool name
schema_digest         = digest of the published input/output contract
dispatch_policy_ref   = reference to the approved dispatch policy
```

Add a dedicated workflow-to-tool binding rather than requiring
`skill_workflow_t`. A proposed `workflow_tool_binding_t` contains:

| Field | Purpose |
|-------|---------|
| `host_id` | Tenant boundary. |
| `tool_id` | Agent-facing tool identity in `tool_t`. |
| `wf_def_id` | Canonical workflow definition. |
| `definition_digest` | Exact published workflow snapshot expected by the gateway. |
| `invocation_mode` | `sync` or `async`. |
| `sync_wait_ms` | Maximum gateway wait for a synchronous result. |
| `total_deadline_ms` | Maximum end-to-end workflow deadline. |
| `execution_class` | Default scheduler class for direct/root invocation; the initial synchronous profile uses `interactive`, while nested invocation inherits its outer class. |
| `result_text_mode` | Non-executable MCP text rendering mode: `compact-json` or schema-backed `summary`. |
| `idempotency_policy` | Required key, derived business key, or read-only handling. |
| `delegation_policy` | Allowed nested tool references, audiences, and maximum depth. |
| `response_policy_digest` | Classification and filtering policy snapshot used for later result reads. |
| `aggregate_version` | Optimistic concurrency and event projection version. |
| `active` | Publication lifecycle state. |

The binding must reference one immutable workflow version and digest. Editing a
published workflow creates or promotes a new version; it must not silently
change the implementation behind an existing digest.

The optional relationships are:

```text
skill_t
  -> skill_tool_t -> workflow-backed tool_t
  -> skill_workflow_t -> wf_definition_t

tool_t
  -> workflow_tool_binding_t -> wf_definition_t
```

The two paths may point to the same workflow, but neither path copies the
workflow DSL.

Store each resolved nested dependency in a separate
`workflow_tool_dependency_t` projection keyed by the outer binding and nested
stable tool reference. It records the nested version, contract digest,
compatibility policy, logical authorization tool name, endpoint key, and
authorization-policy reference. A reverse index on the nested stable reference
is required so publication can report every affected composite tool and
require revalidation or reapproval before an incompatible nested contract is
promoted or the nested tool is retired.

## Projected Gateway Configuration

Keep `apiType` as the backend transport dimension (`http` or `mcp`). Select a
workflow-backed tool through the existing catalog concept
`executionPlacement: workflow`; do not add `workflow` as a third transport.
The gateway runtime therefore dispatches by execution placement first and uses
`apiType` only for gateway-executed backend calls.

Example projected tool for the later write-capable profile; the Phase 1 variant
must be read-only:

```yaml
- name: recommend_customer_offer
  description: Recommend and record the best eligible customer offer.
  method: call
  executionPlacement: workflow
  endpoint: recommend_customer_offer@call
  inputSchema:
    type: object
    additionalProperties: false
    required:
      - requestId
      - customerId
      - channel
    properties:
      requestId:
        type: string
        description: Stable business request identifier used for idempotency.
      customerId:
        type: string
      channel:
        type: string
  outputSchema:
    type: object
    additionalProperties: false
    required:
      - status
      - customerId
    properties:
      status:
        type: string
        enum:
          - APPROVED
          - REJECTED
          - NO_CONSENT
          - NO_ELIGIBLE_OFFER
      customerId:
        type: string
      selectedOfferId:
        type: string
      decisionId:
        type: string
  workflow:
    wfDefId: 2695cdee-cb82-4b34-a2d8-f69093c733e3
    version: 1.0.0
    definitionDigest: sha256:0123456789abcdef
    mode: sync
    executionClass: interactive
    waitTimeoutMs: 20000
    totalDeadlineMs: 30000
    maximumDefinitionTasks: 8
    maximumExecutionAttempts: 8
    maximumNestedCalls: 8
    maximumParallelism: 1
    maximumDelegationDepth: 1
    idempotencyInput: requestId
    resultReplayMs: 300000
    resultTextMode: compact-json
  toolMetadata:
    routing:
      domain: Offers
      semanticNamespace: customer-offers
      semanticDescription: Recommend an eligible personalized customer offer.
      semanticKeywords:
        - recommend offer
        - personalized offer
        - customer eligibility
      sensitivityTier: confidential
    safety:
      read_only: false
      idempotent: true
      destructive: false
      humanApprovalRequired: false
    runtime:
      costTier: medium
      estimatedLatencyMs: 5000
    lifecycle:
      version: 1.0.0
      status: active
```

Replace the placeholder `definitionDigest` with the canonical digest of the
exact workflow definition being pinned:

```bash
cargo run -p light-workflow --example workflow_definition_digest -- <definition.yaml>
```

The configuration contains only an approved reference, contract, and bounds.
It does not contain executable scripts or caller-selectable destinations.
The five-minute `resultReplayMs` in this later write-capable example is an
illustrative business-idempotency policy, not a platform or read-only default.
Read-only tools normally publish a much shorter completed-result freshness
window, including zero when only in-flight deduplication is wanted.

The metadata ownership and compact `tools/list` rules in
[MCP Tool Metadata Usage](mcp-tool-metadata-usage.md) continue to apply.
The existing metadata contract intentionally uses `read_only`, `idempotent`,
and `destructive` together with `humanApprovalRequired`; projection must retain
those canonical spellings rather than normalize them ad hoc.

## Workflow Invocation API

The gateway needs a stable internal API instead of calling `/portal/command`,
polling Portal query handlers, or querying workflow tables.

Required operations:

```text
POST   /v1/workflow-invocations
GET    /v1/workflow-invocations/{workflowInstanceId}
GET    /v1/workflow-invocations/{workflowInstanceId}/result
POST   /v1/workflow-invocations/{workflowInstanceId}/wait
DELETE /v1/workflow-invocations/{workflowInstanceId}
```

The API may initially adapt the existing command/event/query implementation,
but those details remain behind the workflow service boundary described in
[Workflow Client Architecture](../../design/workflow-client-architecture.md).

### Start Request

```http
POST /v1/workflow-invocations
Authorization: Bearer <workflow-delegation-token>
Idempotency-Key: <gateway-derived-key>
X-Correlation-Id: <correlation-id>
Content-Type: application/json
```

```json
{
  "stableToolRef": "019f0000-0000-7000-8000-000000000001",
  "workflowRef": {
    "wfDefId": "2695cdee-cb82-4b34-a2d8-f69093c733e3",
    "version": "1.0.0",
    "definitionDigest": "sha256:0123456789abcdef"
  },
  "mode": "sync",
  "deadlineTs": "2026-08-12T20:00:30Z",
  "input": {
    "requestId": "OFFER-REQUEST-9001",
    "customerId": "CUST-1001",
    "channel": "portal"
  }
}
```

Tenant and caller identity come from the authenticated delegation token. If a
tenant identifier is also carried in the body or transport metadata, it must
match the authenticated identity and fail closed on disagreement.

### Durable Start Path

The invocation service allocates `workflowInstanceId` before durable
acceptance. In one database transaction it must:

1. reserve the idempotency key under its unique scope;
2. store the normalized input digest, caller binding, definition, policy, and
   response-filter snapshots;
3. create the process and initial task rows; and
4. append an invocation-accepted audit/projection event to the outbox.

`POST /v1/workflow-invocations` returns the allocated instance ID only after
that transaction commits. `GET /v1/workflow-invocations/{id}` must immediately
return at least `ACCEPTED`; it must never return `404` merely because an
asynchronous projection has not caught up.

Gateway-initiated synchronous starts must not depend on consuming the shared,
ordered Portal event log. The current consumer claims global offset ranges and
rolls back a complete batch when event handling fails, so unrelated backlog or
a poison event could otherwise consume the interactive wait budget or block a
tenant partition indefinitely. The acceptance event emitted above is for
audit and projections; it is not the command that creates the process. Use a
distinct event type, or make its handler explicitly recognize a pre-created
instance, so it cannot start a duplicate workflow.

Event consumers still require poison-event isolation for non-start
projections. Deterministic parse, schema, and contract failures are poison and
are quarantined without immediate same-transaction retries. Retryable database
and transport failures roll back the claimed batch and use the consumer's
outer reconnect backoff; they must never consume poison attempts or block an
aggregate in quarantine. A permanently invalid event is recorded in a
tenant/partition-scoped quarantine with its offset, error, and payload digest
in the same transaction that advances the consumer offset. The
quarantine or deferred-event store also retains the encrypted replayable
payload, or a durable immutable payload reference, plus every source offset and
aggregate version needed to reconstruct order. If replay depends on
`outbox_message_t`, outbox retention must exceed the maximum quarantine dwell
and unresolved holds must block purging those offsets.

The affected aggregate is marked blocked so later events for that aggregate
are deferred or quarantined in order, while unrelated aggregates continue. The
consumer raises an operational alert and supports an audited, ordered repair-
and-replay operation. A malformed unrelated event must not block workflow
start, status, wait, or result retrieval.

### Invocation State

Use a small stable state model at the API boundary:

```text
ACCEPTED
RUNNING
WAITING
COMPLETED
FAILED
CANCELLED
```

The response includes the workflow instance ID, current state, definition
digest, timestamps, retryability, and a public result or normalized error when
terminal. It never returns the full internal context, credentials, or
unfiltered intermediate task outputs.

`POST /{id}/wait` is a bounded, resumable long poll over durable instance
state, not an in-memory gateway subscription. Any authorized gateway node can
resume it after a restart. Multiple waiters may observe the same instance and
must not change its state. The effective server wait is the minimum of the
published `sync_wait_ms`, the service-side long-poll cap, and the remaining
workflow deadline. A timeout returns the latest state and instance ID; it does
not imply cancellation or failed acceptance.

The invocation service bounds dedicated PostgreSQL `LISTEN` connections with
`WORKFLOW_WAIT_LISTENER_CONNECTIONS` (default 8). Additional waiters use short
durable status polling, so synchronous permit capacity cannot translate into
an unbounded database-connection count.

### Public Result

`light-workflow` must produce the public result only from the canonical
workflow `output` definition. Executable result expressions do not belong in a
binding row. The public result is validated against the workflow output schema
before the instance is reported as `COMPLETED`.

The gateway then applies its current response-filter and output-validation
pipeline. A workflow must not return raw backend transport envelopes directly
to the agent.

### MCP Result Envelope

For Phase 1, workflow-backed tools must publish an object-root output schema.
After workflow validation and gateway response filtering, the gateway emits
the filtered object unchanged as `structuredContent`; this is the authoritative
machine-readable result and the value validated against `outputSchema`.
Array or scalar public outputs must be modeled explicitly inside an object,
such as `{ "items": [...] }`, rather than receiving an implicit runtime wrapper.

The published contract selects one non-executable text rendering mode:

- `compact-json` is the compatibility default and emits one text content block
  containing a compact serialization of the filtered `structuredContent`.
- `summary` requires a schema-declared, required `summary` string in the same
  public result and emits exactly that field as the text content block.

The `summary` mode is preferred for large structured results because it avoids
duplicating the entire result in the model-visible text channel. Rendering can
never read hidden workflow context or introduce data absent from the filtered
`structuredContent`. Text and structured output limits are publication-time
and runtime gates.

A technical failure sets `isError: true`, omits success
`structuredContent`, and returns a concise sanitized text block containing the
stable error code and workflow instance ID when allocated. The same code,
instance ID, state, and retryability are carried in bounded gateway `_meta` for
programmatic clients. Business outcomes remain successful structured results.

## Gateway Execution Flow

For `tools/call`, the gateway performs:

1. Resolve the gateway-facing name to one immutable tool and workflow binding.
2. Apply tools-call authorization using the composite tool endpoint key.
3. Validate arguments against the published input schema.
4. Mask or transform arguments according to approved request policy.
5. Acquire a synchronous-wait permit from depth zero or the signed delegated
   depth pool when required.
6. Derive the idempotency key and construct correlation, deadline, invocation-
   budget, effective execution-class, and delegation context.
7. Start the workflow using the pinned definition version and digest.
8. Wait only when the tool is published as synchronous.
9. Map workflow state and public output into an MCP tool result.
10. Apply response filtering for the initiating caller.
11. Validate successful structured content against `outputSchema`.
12. Emit gateway and workflow correlation/audit attributes.

If the invocation service reports a different workflow digest, tenant, stable
tool reference, or schema binding, the gateway fails closed.

## Synchronous Tools

Synchronous tools preserve the simplest compatibility contract: an existing
agent issues one `tools/call` and receives the business result.

The initial synchronous profile should allow only bounded, headless workflows.
Recommended starting limits are:

| Limit | Initial value |
|-------|--------------:|
| Static definition tasks | 8 |
| Runtime task attempts, including retries | 8 in Phase 1 |
| Nested tool/API calls | 8 |
| Parallel branches | 1 until fork/join is qualified |
| Nested workflow-tool depth | 1 |
| Gateway wait | 20 seconds |
| Total workflow deadline | 30 seconds |

These are starting defaults, not protocol constants. They should be
environment-configurable and publication-validated.

Static definition size and runtime execution consumption are separate
budgets. Publication counts all reachable tasks, including nested composite
dependencies. At invocation time the gateway creates one structured budget
covering remaining task attempts, nested calls, delegation depth, parallel
branches, request/intermediate/result bytes, wall-clock deadline, and optional
cost. The signed delegation token identifies the invocation and carries the
immutable budget ceilings, but it is not the mutable counter. The invocation
service maintains one durable, atomic budget ledger shared by every task,
retry, and parallel branch. Before dispatch, a worker conditionally reserves
attempts, calls, bytes, and cost from that ledger in one transaction; dispatch
is refused if any remaining counter is insufficient. Actual byte and cost
usage reconciles a bounded reservation by idempotent, fenced updates so a
crash or duplicate completion cannot release or consume it twice. Fork/join
may instead pre-split non-overlapping child reservations, but copied tokens
must never create additional budget. Retries consume the same invocation
ledger rather than resetting the envelope. Request, intermediate, and result
byte ceilings remain distinct: both
gateway and invocation service enforce request bytes, the executor consumes
intermediate bytes, and public-result construction enforces result bytes.

The first profile should allow deterministic API/MCP/rule calls, `set`,
`switch`, and `assert`. It should reject human `ask` tasks, unbounded model
calls, runner tasks, schedules, and unbounded loops.

Synchronous eligibility is transitive. Portal validation walks the complete
dependency graph and rejects a synchronous tool if any reachable composite is
asynchronous, contains a human or unbounded task, exceeds the aggregate
invocation budget, or can outlive the outer deadline.

When the gateway wait expires, it returns an MCP error result containing a
machine-readable workflow instance ID, state, and retryability. The workflow
may continue durably unless its published cancellation policy says otherwise.
The gateway must never silently start a second instance when the caller retries
and resolves to the same gateway-derived idempotency key.

Each active synchronous wait consumes a gateway workflow-concurrency permit.
When the global, tenant, or tool-specific permit pool is exhausted, the
gateway returns a retryable `WORKFLOW_CAPACITY_EXHAUSTED` response before
starting an instance; it does not queue the call behind an unbounded wait.

Nested synchronous calls must not reacquire from the same pool held by their
outer waits. This design reserves a separate, non-borrowable permit pool for
each allowed delegation depth. A root call acquires from depth zero; a valid
internal call to another synchronous workflow-backed tool increments the
signed call depth and acquires from the corresponding inner pool. Ordinary
HTTP or non-composite MCP backend calls do not consume workflow-wait permits.
Depth-zero calls cannot consume inner reserves, and ordinary callers cannot
claim an inner depth. Global, tenant, and tool limits still apply within each
pool.

Every enabled synchronous delegation depth must have explicit non-zero
capacity. Portal publication rejects a synchronous dependency graph whose
maximum depth has no configured pool, and gateway admission fails the outer
call before durable start if a required depth pool is absent, disabled, or
unhealthy in the active capacity profile. Separate pools prevent a saturated
set of outer waits from holding every permit needed by their own nested calls;
they do not pre-reserve one inner permit per outer call. Exhaustion within an
inner pool therefore remains a bounded overload failure, not a circular wait.

### Interactive Execution Class And Scheduling

Fast durable acceptance is necessary but does not satisfy the synchronous SLO.
The result deadline includes durable start, every task queue delay, backend
execution, state transitions, final-result construction, gateway filtering,
and response rendering.

At outer admission, select one immutable effective execution class for the
complete invocation chain. The initial agent-facing synchronous profile uses
`interactive`. For a root invocation, the binding supplies the default. For an
internal invocation, the signed delegation context supplies the effective
class and the nested binding's value is only its direct-root default. All
descendant tasks and nested workflow invocations inherit that effective class.
An agent cannot submit, raise, or spoof it. `standard` and `batch` classes use
separate capacity shares. Batch workers may borrow unused interactive
capacity, but interactive work must be able to reclaim its reserved share
without preempting an already running side effect.

Priority alone is insufficient because the current host-task claim is global
across tenants. The scheduler must provide bounded per-tenant concurrency and
fair selection within each execution class, with aging so a continuously busy
tenant or priority tier cannot starve another tenant indefinitely. Horizontal
replicas may share the database queue, but their claim protocol and indexes
must preserve the same fairness contract rather than reverting to a global
oldest-row race.

Task insertion and transition commits must wake eligible executors. A suitable
PostgreSQL implementation establishes `LISTEN` before catch-up, drains claims
until empty, waits for a non-sensitive `NOTIFY`, and retains a short fallback
poll for missed notifications and recovery. The current 500 ms sleep occurs
after an empty claim, so it is primarily an idle-to-active dispatch penalty,
not automatically a 500 ms charge for every sequential hop. End-to-end tests
must nevertheless measure every queue interval because other tenants' tasks
can be selected between transitions.

Executor capacity is a first-class deployment and admission dimension:

- configurable concurrent host-task workers per service instance;
- horizontally scalable service replicas and scheduler partitions;
- reserved interactive workers plus per-tenant and per-tool limits;
- measured queue depth, oldest runnable age, claim latency, service time, and
  available interactive slots; and
- admission that rejects before acceptance when the remaining deadline cannot
  be met with the currently advertised interactive capacity.

Gateway wait permits must be coordinated with workflow executor capacity. A
deployment must not admit hundreds of synchronous waits merely because gateway
connections are available while only one workflow task can execute.

Replace the host task's Boolean lock and fixed five-minute recovery window with
a renewable lease containing `lease_id`, monotonically increasing
`fencing_token`, `lease_expires_ts`, and worker identity. Task completion and
transition writes succeed only when the lease and fencing token still match.
For interactive work, the initial lease and every renewal are capped by the
remaining workflow deadline. Expired tasks are reclaimed only while useful
work can still finish; otherwise they transition to a stable deadline failure.
Lease duration, renewal interval, and crash-recovery target must be materially
shorter than the synchronous deadline and tested with executor termination.

Fencing prevents a stale worker from committing workflow state, but it cannot
undo or deduplicate an external side effect. Phase 1 remains read-only; later
write-capable tasks also require the protections in
[Idempotency And Side Effects](#idempotency-and-side-effects) and the durable
`none`/`possible`/`confirmed` state defined in
[Failure Mapping](#failure-mapping) before lease-based re-execution is allowed.
A reclaimed task in `possible` or `confirmed` state cannot automatically repeat
the call unless the downstream idempotency contract proves that replay is safe.

## Asynchronous Tools

Use asynchronous publication for workflows that may:

- wait for a human decision;
- call an agent or runner with an uncertain duration;
- perform a long fan-out or batch operation;
- continue for longer than the interactive gateway deadline; or
- require cancellation or compensation after the initiating call returns.

The declared output schema returns a handle:

```json
{
  "workflowInstanceId": "019f0000-0000-7000-8000-000000000002",
  "status": "ACCEPTED",
  "submittedAt": "2026-08-12T20:00:00Z"
}
```

Expose generic MCP tools for lifecycle operations:

```text
workflow_get_status
workflow_get_result
workflow_cancel
```

These tools use the same tenant and caller authorization boundary. A caller
cannot discover or control another caller's instance merely by obtaining an
instance ID.

Bind instance access to both the authenticated service principal and the
initiating end-user subject/actor claim when one exists. A shared service
principal alone is not sufficient isolation. The instance stores the
publication-time classification and response-filter policy snapshot so
`workflow_get_result` can reproduce the approved disclosure boundary after the
original MCP session ends.

The snapshot is a maximum disclosure ceiling, not a frozen authorization
grant. Every lifecycle call resolves the principal and end-user subject's
current claims, revocation state, and tenant access, then evaluates the current
authorization policy. Result rendering applies the more restrictive
intersection of that current decision and the stored classification/filter
snapshot. A user who has lost access is denied; a later policy change cannot
broaden what the accepted instance was allowed to disclose.

Token refresh must not revoke an otherwise unchanged lifecycle identity. The
gateway therefore hashes stable authorization and data-boundary claims while
excluding volatile JWT lifecycle fields such as `exp`, `iat`, `nbf`, `jti`,
and `nonce`. A change to roles, scopes, tenant, subject, or any other retained
boundary claim changes the digest and fails closed.

Expose the three lifecycle tools when the authenticated caller's tenant has at
least one active asynchronous composite tool or the caller can access an
active or retained workflow instance. Retiring the tenant's last asynchronous
composite must not remove status, result, or cancel from `tools/list` while an
authorized instance remains discoverable under the retention policy. The
existence check applies the same principal, initiating-subject, tenant, and
current-authorization rules as the lifecycle calls; an inaccessible instance
must not make the tools visible. This avoids expanding every tenant's
`tools/list` surface while preserving lifecycle access after retirement.

Do not make one tool unpredictably return either a business result or an async
handle unless its published output schema explicitly models both outcomes.
Changing a published tool between synchronous business-result and asynchronous
handle semantics is a breaking contract change. It requires a new stable tool
identity and gateway-facing name rather than an alias rebind.

## Underlying API And MCP Calls

A workflow may call registered APIs directly or invoke existing gateway MCP
tools.

The preferred default for a composite MCP tool is a workflow `call: mcp` using
approved, pinned gateway tools because this preserves:

- stable tool identity;
- gateway service discovery and argument mapping;
- fine-grained authorization and response filtering;
- shared audit and diagnostics; and
- consistent MCP/API behavior.

Direct HTTP workflow tasks are appropriate when workflow policy explicitly
authorizes a registered service endpoint and service identity. The workflow
must not accept a destination URL from an agent or transform expression.

At publication time, resolve each nested tool reference to:

```text
stableToolRef
gateway-facing name
tool version and contract digest
contract compatibility class
logical authorization tool name and endpoint key
authorization-policy reference
lifecycle status
```

The workflow definition digest is always an exact immutable pin. Nested tool
contracts use versioned dependency resolution: an outer composite continues to
dispatch the approved nested version after a newer alias is published, so an
unrelated inner publication cannot cause an outer runtime outage. An optional
`follow-compatible` policy may advance only when Portal proves that the new
nested input accepts every previously valid outer request, the new output is
within the contract the outer mapping expects, and authorization or data-
classification policy has not broadened. Because general JSON Schema
compatibility is not decidable for every schema, unsupported or ambiguous
changes are incompatible and require an explicit outer repin, conformance
test, and reapproval.

Portal uses the dependency reverse index to show the inner publisher the
affected composites before promotion. Security revocation can still invalidate
a pinned dependency immediately; ordinary version promotion cannot.

Phase 1 therefore requires version-aware internal dispatch, not alias lookup.
The control plane projects a private dependency-target registry alongside
`mcp-router.tools`, keyed by `stableToolRef`, tool version, and contract digest.
The public alias exposes only the currently promoted version through
`tools/list`; a workflow delegation token invokes the pinned private target by
stable reference and version. Private targets reuse the same authorization,
argument masking, backend dispatch, response filtering, schema validation, and
audit pipeline, but cannot be invoked by an ordinary external tool name.

Authorization identity belongs to the logical tool; dispatch identity belongs
to the version target. Every private target therefore carries the logical
public tool name and the exact endpoint key used by its alias, such as
`accounts@call`, and passes those values through the existing authorization and
response-filter pipeline. Its registry key, private target name, version, and
digest never derive a new endpoint key or new rule binding. This is required
for `defaultDeny: true`, where an unrecognized endpoint or one without request
rules is denied.

An approved version may change backend resolution and contract digest without
changing that logical authorization identity. A version that needs a different
endpoint key, request rule set, permission boundary, or response-filter policy
represents a different capability. Portal classifies it as incompatible and
requires an explicit outer repin, conformance tests, and approval rather than
silently minting a version-specific authorization identity. Current security
revocation continues to take precedence over any pin.

Superseded and retirement-candidate targets remain dispatchable while
referenced by any active composite binding, in-flight workflow snapshot,
rollback window, or required audit/replay retention. Portal maintains reference
counts or equivalent durable reachability evidence. Retirement and garbage
collection use that same reachability index. Garbage collection is allowed
only after all such references and retention holds are gone, and removal is
itself projected and audited. This makes the claimed version pin executable in
Phase 1 rather than depending on a future API.

## Delegation And Cycle Prevention

The gateway issues a short-lived workflow-task delegation token containing or
binding:

- tenant and initiating principal;
- outer stable tool reference;
- allowed nested stable tool references;
- allowed audiences and operations;
- input/data-boundary digest;
- correlation ID;
- immutable effective execution class and current synchronous permit depth;
- structured invocation budget for deadline, task attempts, nested calls,
  depth, parallelism, bytes, and cost, plus the identifier and generation of
  the shared durable budget ledger that owns the mutable counters;
- idempotency context; and
- remaining delegation depth.

Nested calls can only narrow rights and budget reservations. They cannot extend the
initiating deadline, add tools, broaden the data boundary, change tenant, or
select their own scheduling class. When dispatching another workflow-backed
tool, the gateway increments permit depth and copies the effective execution
class into the nested token. It accepts these claims only from a gateway-issued
delegation token; an external MCP request always enters at depth zero and uses
its root binding's class. Token verification authenticates the immutable
ceiling and ledger identity; every mutable consumption decision is an atomic
conditional update against that ledger, never a decrement trusted from token
contents.

Portal publication builds a dependency graph for every workflow-backed tool.
It rejects:

- a workflow that calls its own composite tool;
- a cycle across two or more workflow-backed tools;
- a call to an unbound or retired tool;
- a nested call whose approved version or contract digest is unresolved or
  incompatible; and
- a path that exceeds the configured maximum delegation depth.

The runtime also enforces the depth and allowed-tool set so a stale or malicious
definition cannot bypass publication checks.

## Authorization And Data Protection

Authorization happens at two levels:

1. The gateway authorizes the caller to invoke the composite business tool.
2. Each workflow task is authorized for its specific underlying API or MCP
   tool using a narrowed delegation or approved workflow service identity.

The first authorization does not imply unrestricted access to every tool used
by the workflow. The binding and workflow policy define the exact internal
capability set.

Nested identity is declared per step and defaults to narrowed initiating-user
delegation. Workflow service identity is permitted only when the step has
explicit publication-time approval evidence because it can authorize an
operation the initiating user could not perform directly. Service identity
must remain tenant-bound, tool-bound, deadline-bound, and no broader than the
published capability set.

The [MCP Tools Access Control](mcp-tools-access-control.md) response-filtering
boundary still applies to the final result. Intermediate workflow context and
task outputs need their own classification and redaction rules because they
may contain more data than the final caller is allowed to receive.

Required protections include:

- registered endpoint and workflow references only;
- no arbitrary URL, credential, or service-id arguments;
- encrypted secret references rather than credentials in workflow YAML;
- bounded request, intermediate context, task output, and final output sizes;
- redaction before logs, events, traces, and AI authoring context;
- tenant-bound workflow instance lookup;
- fail-closed schema and digest mismatches; and
- explicit approval policy for destructive or high-impact tools.

## Idempotency And Side Effects

Do not depend on an agent to generate or replay a correct idempotency key. For
read-only and ordinary synchronous calls, the gateway derives the key from the
tenant, authenticated principal and end-user subject, stable tool reference,
workflow definition digest, and canonical effective input. Because input and
definition digests participate in this derived key, a different input or
version intentionally creates a different invocation; it is not an
idempotency conflict.

A client-provided `Idempotency-Key`, explicit business-key input such as
`requestId`, or configured business-key expression is accepted only when the
published policy allows it. The gateway scopes and hashes that untrusted key
with tenant, trusted identity, and stable tool fields, and stores the effective
input and definition digests beside it. Reusing an explicit scoped key with
different input or definition produces `WORKFLOW_IDEMPOTENCY_CONFLICT`.

Side-effecting workflows require a stronger business idempotency contract
because two intentionally distinct operations may have identical arguments.

The publication UI must require one of:

- an explicit idempotency input field;
- a deterministic business-key expression;
- an upstream server-enforced idempotency key; or
- a declaration that duplicate effects are impossible or compensated, with
  approval evidence.

The workflow invocation service stores the accepted key with the stable tool
reference, workflow digest, trusted identities, and effective-input digest.
The database enforces one current reservation with a unique constraint over
tenant, authenticated principal/end-user subject, stable tool reference, and
the final scoped key. Definition and input digests are stored values, not part
of that uniqueness key, so an explicit-key conflict can be detected. The
implementation uses one atomic insert/conflict or compare-and-swap path rather
than read-then-write.

Separate two time windows:

- **In-flight deduplication** lasts until the instance reaches a terminal state
  or its maximum deadline and uncertain-outcome retry grace have elapsed. A
  duplicate returns the existing instance.
- **Completed-result replay** is a separate publication-time freshness policy.
  Before `result_replay_until`, an identical request returns the completed
  instance and result. After it expires, an atomic reservation-generation
  change starts a new instance while retaining immutable history.

Read-only tools should normally use a short completed-result replay window so
repeated questions can observe fresh data. Write-capable tools require a
window consistent with their downstream side-effect idempotency and retry
contract. Retention of invocation and audit history is independent of whether
the active reservation can advance to a new generation.

Canonical effective input is the schema-validated workflow input after
approved deterministic defaults and request mappings, before logging
redaction. It uses a versioned JSON Canonicalization Scheme profile based on
[RFC 8785](https://www.rfc-editor.org/rfc/rfc8785.html): duplicate object keys
are rejected, object properties are recursively sorted by the RFC's UTF-16
ordering, array order is preserved, and finite numbers use the specified
deterministic representation. Values outside the interoperable IEEE 754 range,
including large integer identifiers, must be schema-declared strings. Absent
properties remain absent and therefore differ from explicit `null`. Unicode
string code points are preserved exactly; NFC or other Unicode normalization
is not applied. These rules and the profile version are pinned by conformance
fixtures shared by Portal, gateway, and invocation service.

Event-source deduplication, such as a unique source event ID used while
projecting `WorkflowStartedEvent`, remains a separate safeguard and does not
satisfy caller-invocation idempotency.

For multi-step writes, the workflow definition owns compensation. The gateway
does not attempt to reverse completed backend operations.

## Failure Mapping

Keep business outcomes separate from technical failures.

Business outcomes such as `NO_CONSENT` or `NO_ELIGIBLE_OFFER` are successful,
schema-valid tool results. Technical failures produce `isError: true` and a
stable machine-readable class such as:

```text
WORKFLOW_INPUT_INVALID
WORKFLOW_START_REJECTED
WORKFLOW_DEFINITION_MISMATCH
WORKFLOW_TIMEOUT
WORKFLOW_CANCELLED
WORKFLOW_TASK_FAILED
WORKFLOW_OUTPUT_INVALID
WORKFLOW_OUTPUT_INVALID_AFTER_EFFECT
WORKFLOW_POLICY_DENIED
WORKFLOW_CAPACITY_EXHAUSTED
WORKFLOW_INVOCATION_UNAVAILABLE
WORKFLOW_IDEMPOTENCY_CONFLICT
```

The error envelope should include the workflow instance ID when one exists,
whether retry is safe, and a correlation ID. It must not expose credentials,
raw internal errors, hidden task inputs, or backend responses that have not
passed disclosure policy.

The workflow tracks whether externally visible side effects are `none`,
`possible`, or `confirmed`. If public-result construction or output validation
fails after a confirmed effect, return
`WORKFLOW_OUTPUT_INVALID_AFTER_EFFECT` with `retryable: false` and the instance
ID. This is operationally distinct from a pre-effect validation failure; an
agent must not repeat the write merely because its result was undeliverable.

## Transformation And Aggregation Language

Use the workflow DSL as the only authoring contract. Do not invent a gateway
mapping language for composite tools.

Use CEL as the canonical expression language for workflow conditions and data
transformations. This keeps one expression contract across Light-Fabric rules,
gateway policies, workflow authoring, Portal validation, and AI generation.
CEL is not limited to boolean decisions: an expression can also construct
lists, maps, and JSON-compatible objects. The rule engine can retain its
boolean-only contract while the workflow adapter accepts a typed value.

### CEL And jq Comparison

| Concern | CEL | jq |
|---------|-----|----|
| Primary fit | Policy, conditions, validation, routing, and computed values. | JSON extraction, reshaping, pipelines, and complex aggregation. |
| Validation | Can parse and type-check against declared variables and functions before publication. | Dynamically evaluated; schema and type mistakes normally surface during execution. |
| Result model | Produces one typed value. | A filter can produce zero, one, or many streamed values. |
| Collection support | Provides `map`, `filter`, `exists`, and `all`; sufficient for common mappings. | Provides concise `sort_by`, `group_by`, `unique_by`, `reduce`, and recursive traversal. |
| Safety model | Side-effect-free and terminating, but nested collection macros still need cost limits. | Recursion, `while`, and `repeat` require time, memory, depth, and output limits. |
| Platform cost | Reuses the existing Light-Fabric language, evaluator experience, security profiles, and Portal contract. | Adds another runtime, validator, editor mode, security profile, compatibility contract, and AI prompt. |

CEL therefore provides the better default for a governed platform. jq is more
ergonomic for some advanced JSON transformations, but that advantage does not
justify exposing two interchangeable languages throughout every workflow.

References:

- [CEL overview](https://cel.dev/overview/cel-overview)
- [CEL language definition](https://github.com/cel-expr/cel-spec/blob/master/doc/langdef.md)
- [jq manual](https://jqlang.org/manual/)

### CEL Runtime Contract

Provide a shared CEL execution core with location-specific adapters:

- a predicate adapter that requires `bool` for rules, `when`, `switch`, retry
  predicates, and assertions; and
- a value adapter that converts one CEL result to JSON for task inputs,
  exports, derived values, joins, and the final public output.

The existing rule boundary remains unchanged. CEL rule conditions decide
whether declarative actions execute; they do not directly mutate a response or
become a general workflow runtime. Reuse the compiler, type environment,
security validation, value conversion, cost accounting, and diagnostics rather
than coupling workflow execution to the rule engine's boolean API.

The workflow CEL environment exposes only immutable, documented roots:

| Root | Contents |
|------|----------|
| `input` | Schema-validated workflow invocation input. |
| `context` | Accumulated workflow state and approved task exports. |
| `task` | Current task input or result where the expression location permits it. |
| `workflow` | Bounded identifiers and execution metadata, never credentials or secret values. |

Each expression location declares its required result category and, when
available, its JSON Schema-derived type. Portal publication must parse and
type-check the expression against that environment, reject undeclared roots or
functions, and persist the normalized expression and digest with the immutable
workflow version. Runtime execution uses the same environment declaration and
a cached compiled program; it must not reinterpret an expression under a
different profile.

The production CEL profile must enforce:

- allowlisted roots, functions, and collection macros;
- no I/O, network access, mutation, service lookup, or dynamic code loading;
- expression-size, evaluation-cost, collection-size, nesting-depth,
  execution-time, and result-size limits;
- explicit guards for missing or nullable data where required; and
- deterministic failure when evaluation returns the wrong type or cannot be
  converted to exactly one JSON value.

The CEL value adapter initially needs to support:

- selecting fields;
- reshaping objects and arrays;
- joining previously exported task results;
- computing derived values;
- filtering collections; and
- constructing the public output.

### CEL To JSON Contract

The value adapter is new production code, not a thin rename of the existing
boolean evaluator. The compile cache, reference inspection, guarded execution,
and JSON-to-CEL context conversion are reusable; the reverse conversion and
result contract require their own implementation and conformance suite.

The initial cross-runtime CEL-to-JSON mapping is:

| CEL value | JSON representation |
|-----------|---------------------|
| `null`, `bool`, `string` | Corresponding JSON value. Missing remains distinct from explicit `null`. |
| `int`, `uint` | JSON number only within the interoperable ranges `[-9007199254740991, 9007199254740991]` for `int` and `[0, 9007199254740991]` for `uint`; authors must explicitly convert larger identifiers or counters to strings. |
| `double` | Finite JSON number; `NaN`, positive infinity, and negative infinity are rejected. |
| `bytes` | Standard padded Base64 string and a schema that declares the encoding. |
| `timestamp` | UTC RFC 3339 string normalized with a `Z` suffix. |
| `duration` | Protobuf JSON duration string in seconds with optional fractional nanoseconds, for example `"1.500s"`. |
| `list` | JSON array after recursively applying this contract. |
| `map` | JSON object only when every key is a unique string; non-string or colliding keys are rejected rather than stringified. |
| opaque, function, optional-without-value, or implementation-specific values | Rejected unless a later versioned profile defines an explicit conversion. |

The adapter must not silently convert an unsupported value to `null`. The
normalized mapping version is part of the compiled-expression/profile digest
so a library upgrade cannot change persisted results without conformance and
promotion.

Phase 0 must also qualify the concrete evaluator. The currently pinned Rust
`cel` 0.14 API provides parsing, execution, reference inspection, and a generic
JSON helper, but it does not expose the schema-aware checker or evaluation-cost
budget assumed by this design. Its generic JSON helper also stringifies map
keys and chooses dependency-specific representations for types such as
duration. Before Phase 1, either augment or replace that integration with an
implementation that satisfies the checker, cost, and conversion contracts, or
narrow the published CEL profile and validation claims accordingly. Wall-clock
timeouts alone are not a substitute for deterministic evaluation-cost limits.

The current workflow model advertises jq and JavaScript while the runtime
implements only a small jq-like path and comparison evaluator. Before this
profile is published, change the workflow default to CEL, execute expressions
through the shared CEL core, and make Portal and runtime validation reject jq,
JavaScript, and any other unimplemented language.

### Optional Advanced jq Transform

Do not enable jq in the initial production profile. If representative customer
workflows later demonstrate a material need for operations such as grouping,
sorting, reducing, or recursive JSON traversal that would otherwise require
non-portable CEL extensions, jq may be introduced as an explicit advanced
`transform` task. It must not become a per-expression alternative for
conditions, policies, retries, or ordinary mappings.

An optional jq task requires a separate versioned compatibility and security
profile. It must accept one JSON input and return exactly one JSON output;
zero-result and multi-result filters fail unless the task contract explicitly
collects them into one array. The allowed subset must exclude unbounded
recursion and repetition, imports and modules, environment or input access,
and debug or stderr output. The runtime must enforce fuel or cost, time, memory,
depth, input, and output limits.

JavaScript should not be enabled merely because it appears in the workflow
model. When declarative CEL transformations are insufficient and the optional
jq profile is not appropriate, an approved isolated runner task may be used.
Its input, output, image or template digest, resource limit, and execution
policy must be pinned. It cannot execute in the gateway process.

Parallel aggregation requires explicit fork/join semantics with bounded
parallelism and a deterministic merge rule. Step retries require explicit
attempt count, retryable error classes, backoff, jitter, and idempotency
requirements. These semantics belong in `light-workflow`, not in the MCP tool
configuration.

## Portal Authoring Experience

Add a **Composite MCP Tool** workspace to `portal-view`. Reuse the existing
workflow editor, validation, graph, and test-run surfaces.

### Contract

The user defines:

- MCP name, description, semantic metadata, and examples;
- input and output JSON Schemas;
- synchronous or asynchronous mode;
- MCP result text mode and completed-result freshness window;
- latency, deadline, fan-out, and step limits;
- read-only, idempotent, destructive, and approval metadata; and
- target gateway instances or environments.

### Flow

The user can edit YAML directly or assemble a graph from registered API
endpoints, MCP tools, rules, and supported workflow tasks. Selecting a source
operation inserts its stable reference and current schema digest rather than a
free-form URL.

### Mappings

Each task exposes editors for:

- workflow input to call arguments;
- task output to workflow context;
- branch expressions;
- join and aggregation expressions; and
- final public-output mapping.

All expressions use CEL in the initial production profile. The editor shows the
allowed roots and functions for that location, provides schema-aware completion
and syntax/type diagnostics, states the expected result type, and previews the
input and output shape at each step.

### Generate With AI

AI generation is a draft-authoring feature, not a production execution path.
The generation request contains:

- the user's business objective;
- only the APIs and MCP tools selected or authorized for the author;
- their schemas, descriptions, examples, and safety metadata;
- the supported workflow DSL and CEL profile;
- organization policy and runtime bounds; and
- optional sample input and expected output.

The model must not receive credentials or unrestricted catalog access. It must
not invent endpoints, tools, schema fields, or runtime features.

Generation produces:

- a workflow draft;
- a proposed input and output contract;
- dependency and mapping explanations;
- positive, edge, and failure fixtures; and
- assumptions and unresolved questions.

The draft remains unpublished until it passes deterministic validation and a
human approves the diff.

### Validate And Test

The Portal runs, in order:

1. YAML and workflow-schema validation.
2. Runtime-supported task and expression validation.
3. Input/output JSON Schema validation.
4. Stable tool, endpoint, and workflow reference resolution.
5. Schema-digest and lifecycle checks.
6. Dependency-cycle and delegation-depth checks.
7. Safety, approval, idempotency, result-rendering, and data-boundary policy
   checks.
8. Mock fixture tests.
9. Optional live sandbox tests with failure injection.
10. Gateway `tools/list` and invocation qualification against a non-production
    instance.

AI-generated and manually authored definitions use the same validation and
publication pipeline.

## Publication, Versioning, And Rollback

Publication creates an immutable bundle containing:

```text
stable tool reference
gateway-facing alias
input/output schemas and schema digest
workflow definition ID, version, and digest
nested dependency snapshot
dispatch and delegation policy references
response-classification and filtering policy digest
execution class, runtime bounds, result text mode, and replay windows
test evidence
approver and publication metadata
```

The Portal projects the runtime subset into `mcp-router.tools`. The gateway
continues to use last-known-good configuration when a new snapshot is invalid.

Promotion atomically moves the tool alias to the new approved binding. New
calls use the new binding; in-flight workflows continue using their stored
definition and policy snapshots.

Before moving an inner tool alias, Portal queries the dependency reverse index,
classifies the contract change, and shows the affected composite tools. An
incompatible change cannot strand existing outer bindings at runtime: either
the old nested version remains dispatchable, or the outer tools are explicitly
repinned, retested, and reapproved in the same promotion plan.

Retiring an inner tool uses the same reverse-index gate. Portal blocks
retirement while an active outer binding references the tool unless one atomic
plan repins or retires every affected outer binding. Retirement prevents direct
new starts and new dependency publication, but it does not invalidate the
pinned dependency snapshot of a workflow accepted before that plan; its private
target remains dispatchable until the in-flight and retention references are
released. An emergency security revocation is a separate fail-closed operation
and may deliberately break pinned calls with an explicit impact report and
audit record.

Promotion must not change a stable tool between synchronous business-result
and asynchronous handle contracts. That change requires a new stable tool
reference and gateway-facing name so cached schemas and static allowlists do
not observe a semantic type change behind an alias.

Rollback republishes the previous approved binding. It does not mutate or
delete historical definitions or running instances.

Retirement removes the tool from new `tools/list` responses and rejects direct
new starts while preserving the pinned execution dependencies, status, result,
cancellation, and audit access needed by already accepted instances according
to retention policy.

## Observability And Audit

Use one correlation ID across:

```text
outer MCP request
gateway workflow dispatch
workflow instance
workflow tasks
nested gateway/API/MCP calls
final MCP result
```

Recommended gateway span and audit attributes include:

```text
mcp.tool.name
mcp.tool.stable_ref
mcp.tool.endpoint_id
mcp.tool.execution_placement
workflow.definition_id
workflow.definition_digest
workflow.instance_id
workflow.invocation_mode
workflow.execution_class
workflow.permit_depth
workflow.state
workflow.task_count
workflow.nested_call_count
workflow.delegation_depth
workflow.wait_ms
workflow.total_ms
```

High-cardinality identifiers such as `workflow.instance_id`, correlation ID,
and raw digest values belong in spans and audit events, not metric labels.
Metrics use bounded dimensions such as tenant tier, stable tool reference where
cardinality policy permits it, workflow version, state, and normalized error
class.

Metrics should cover:

- starts, completions, failures, cancellations, and timeouts;
- durable-acceptance, runnable-to-claim, task service, transition, synchronous
  wait, result-rendering, and total workflow latency;
- interactive queue depth, oldest runnable age, executor saturation, lease
  expiry/reclaim, and deadline-aware admission rejection;
- active and waiting instances;
- duplicate/idempotent start hits;
- definition, schema, and policy mismatch rejections;
- nested-call denials and cycle/depth rejections;
- output-validation failures; and
- capacity rejection by tenant, tool, workflow version, and bounded permit
  depth.

Do not attach raw inputs, intermediate context, or final results to metrics.
Trace and audit payload capture follows classification and redaction policy.

## Capacity And Availability

The gateway must bound workflow dispatch independently from HTTP and MCP
backend dispatch. Recommended controls include:

- global and per-tenant concurrent workflow starts;
- per-tool and per-delegation-depth concurrent synchronous waits;
- executor-advertised interactive slots, queue age, and throughput estimates in
  admission decisions;
- workflow-invocation connection and response timeouts;
- circuit health for the invocation service;
- request and public-result size limits;
- maximum pending asynchronous instances where policy requires it; and
- overload responses that distinguish safe retry from an accepted workflow.

A gateway timeout must not be reported as "not started" after the workflow
service has durably accepted the instance. The invocation service returns the
instance ID as part of durable acceptance, and retries use idempotency lookup to
resolve uncertain outcomes.

Invocation-service health does not remove an already published tool from
`tools/list`; discovery is a stable contract and may be cached by agents. Calls
fail with retryable `WORKFLOW_INVOCATION_UNAVAILABLE` before acceptance while
the circuit is open. After durable acceptance, errors return the instance ID
and current state instead of an ambiguous unavailable response.

The gateway remains stateless with respect to workflow progress. Gateway
restart or reload does not lose the workflow instance.

## Implementation Phases

Phase 1 is primarily a `light-workflow` runtime qualification project with a
gateway feature attached. Direct invocation, interactive scheduling, fair
claiming, notification wake-up, fenced leases, deadline-aware admission, and
the CEL evaluator decision are release prerequisites rather than follow-up
gateway optimizations. Delivery ownership, staffing, and milestones must
reflect that dependency order.

### Phase 0: Contract And Threat Model

Owners: `light-fabric`, `light-workflow`, `portal-db`, and `light-portal`.

The versioned implementation artifacts live under
`contracts/workflow-invocation/v1`. The shared Rust types and strict
canonicalizer live in `workflow-invocation-contract`; the direct transactional
acceptance boundary is `light-workflow::invocation`; and the matching Portal
schema patch is `patch_20260812_01_workflow_mcp_phase0.sql`. Run
`scripts/run-workflow-mcp-phase0-gates.sh` with a disposable PostgreSQL URL to
verify both repositories. The qualification manifest deliberately keeps
runtime promotion disabled until the exact Phase 1 scheduler/executor topology
and replacement or augmented CEL evaluator produce passing evidence.

- Define the invocation API, state model, public result, and error envelope.
- Define `workflow_tool_binding_t`, the nested-dependency reverse index, the
  invocation/idempotency store, the durable atomic invocation-budget ledger,
  and their command/query events.
- Define exact workflow pins, supported nested-schema compatibility rules,
  dependency impact reporting, private version-target retention/garbage
  collection, and policy digest rules.
- Define delegation claims, depth-partitioned synchronous permit pools,
  inherited execution-class semantics, idempotency semantics, cancellation
  behavior, and synchronous eligibility rules.
- Define the MCP result envelope, canonical-input profile, completed-result
  freshness window, and explicit-key conflict behavior with shared fixtures.
- Spike and load-test the complete interactive result path, including direct
  transactional acceptance, fair task scheduling, wake-up, executor capacity,
  and crash recovery. Durable acceptance must create the invocation, process,
  initial task, snapshots, idempotency reservation, and audit outbox event
  without waiting for the shared event consumer. Measure acceptance separately
  as a necessary sub-gate, but gate the architecture on end-to-end result
  latency.
- Qualify the CEL implementation for schema-aware checking, deterministic cost
  enforcement, and the pinned CEL-to-JSON conversion table. Decide whether to
  augment, upgrade, or replace the current crate before freezing fixtures.
- Publish OpenAPI/JSON Schema fixtures and positive/negative conformance tests.

Exit gates:

- Portal, gateway, and invocation-service fixtures give the same canonical
  digest for reordered objects, numeric spellings, absent versus `null`,
  preserved Unicode, and rejected duplicate keys;
- a committed start returns its preallocated instance ID and an immediate
  status read returns `ACCEPTED` or a later state, never projection-lag `404`;
- numeric acceptance p95/p99 sub-gates and end-to-end result p95/p99 gates are
  selected before Phase 1. The result gate uses controlled one-task and
  maximum-task workflows under concurrent interactive load, cross-tenant batch
  backlog, unrelated outbox traffic, and poison-event injection, with every
  queue and execution stage measured separately;
- the selected result-latency gate is met without tenant starvation, and an
  executor killed after claim is recovered within the remaining interactive
  deadline while its stale fencing token cannot commit;
- CEL conformance fixtures pin large integers, finite/non-finite doubles,
  timestamps, durations, bytes, null versus missing, map keys, opaque values,
  result types, checker behavior, and cost exhaustion;
- stale workflow definitions fail closed, while nested compatible changes and
  incompatible repin requirements follow the documented rules;
- an inner contract change produces a complete reverse-dependency impact
  report before promotion, an existing outer binding continues to call its
  pinned private version, and garbage collection refuses a referenced target;
- cross-tenant start/status/result/cancel tests fail closed;
- concurrent derived-key duplicates resolve to one workflow instance, changed
  derived input starts a new instance, reuse of one permitted explicit key with
  changed input returns `WORKFLOW_IDEMPOTENCY_CONFLICT`, and expiry of the
  completed-result replay window atomically starts a new generation;
- parallel consumers of copied delegation tokens share one ledger: with only
  `N` attempts, calls, bytes, or cost units remaining, at most `N` reservations
  commit, retries do not reset counters, and duplicate or stale fenced
  reconciliation cannot double-release a reservation;
- compact-JSON and summary-mode fixtures pin `content`, `structuredContent`,
  filtering, schema validation, size limits, and technical-error rendering.

### Phase 1: Read-Only Synchronous MVP

Owners: `light-gateway`, `light-workflow`, `light-portal`, and `portal-view`.

- Add the workflow execution-placement dispatch branch and configuration parser
  to the gateway while keeping `apiType` limited to backend transports.
- Add depth-partitioned synchronous permit pools whose inner reserves are
  reachable only through signed workflow delegation.
- Add the direct durable invocation façade and resumable bounded long-poll
  operation; do not drive gateway starts through the shared Portal event log.
- Add bounded poison-event quarantine and audited replay to workflow event
  consumers.
- Add the immutable interactive execution class, fair per-tenant claiming,
  wake-on-insert, configurable concurrent executors, deadline-aware admission,
  and renewable fenced task leases.
- Support headless, bounded sequential compositions.
- Replace the placeholder jq-like evaluator with the shared CEL predicate and
  value adapters, change the workflow default to CEL, and validate expressions
  before publication.
- Add manual Composite MCP Tool authoring, validation, test, and publication.
- Project the private version-target registry and dispatch nested workflow calls
  by pinned stable reference, version, and contract digest rather than alias;
  preserve the logical authorization identity and retirement reachability.
- Use narrowed initiating-user delegation by default for nested MCP calls;
  require approval evidence for each service-identity step.
- Restrict the first profile to read-only operations.

Exit gates:

- an unchanged generic MCP client discovers and calls a composite tool;
- the final result passes gateway response filtering and output validation,
  appears unchanged in `structuredContent`, and uses the published text mode;
- gateway restart does not lose an accepted workflow;
- any gateway node can resume a wait, concurrent waiters observe one durable
  instance, and capacity exhaustion rejects before starting or queueing;
- with `N` permits configured at root and first-nested depth, `N` concurrent
  root workflows that each call one controlled nested composite complete
  without root saturation consuming the nested reserve; direct requests cannot
  claim or spoof an inner-depth permit;
- workflow/gateway traces share one correlation ID;
- the Phase 0 end-to-end p95/p99 result gates continue to pass at the declared
  executor concurrency and cross-tenant backlog, with bounded fairness and no
  starvation;
- an idle executor is woken without waiting for the fallback poll, and an
  executor crash reclaims an interactive task before its deadline while a
  stale completion is rejected by fencing;
- Portal validation and workflow execution agree on CEL conformance fixtures,
  and jq or JavaScript definitions fail publication and runtime loading;
- recursive workflow-tool dependencies and transitively async or unbounded
  dependencies are rejected for synchronous publication;
- a nested workflow inherits the outer effective execution class even when its
  binding has a different direct-root default;
- with `defaultDeny: true`, promoting an inner alias does not change either the
  version or logical authorization identity used by an existing outer binding,
  and its pinned call still passes the alias endpoint's rules without a new
  private-target rule;
- retirement is rejected while active outer references exist unless one plan
  repins or retires them, and referenced or in-flight private targets cannot be
  retired from dispatch or garbage-collected;
- the configured static-task, runtime-attempt, nested-call, payload, cost,
  wait, and total-deadline budgets are enforced by the shared invocation
  ledger rather than mutable token claims;
- a malformed event is quarantined after bounded attempts and cannot block
  start, status, wait, result, or unrelated aggregates in its tenant partition;
  every deferred offset remains replayable, and retention or purge refuses to
  remove its payload dependency.

### Phase 2: Production Orchestration

Owners: `light-workflow` and the workflow client in `light-gateway`.

- Complete deterministic, schema-aware CEL transformations and aggregation
  conformance with cost and result-size enforcement.
- Add bounded fork/join aggregation and generic task retries.
- Add explicit task and workflow deadlines.
- Add asynchronous start/status/result/cancel tools.
- Add side-effect idempotency, approval, and compensation policies.
- Distinguish post-effect output failures as non-retryable and preserve their
  side-effect state in status and audit responses.
- Add dependency-drift checks and version promotion/rollback.
- Use representative customer workflows to decide whether the optional
  restricted jq transform task is justified; keep jq rejected otherwise.

Exit gates:

- parallel partial failure is deterministic and auditable;
- fork/join siblings and concurrent retries carrying copies of the same signed
  parent token cannot exceed aggregate attempt, call, byte, or cost ceilings;
  tests race `N+1` reservations against a remaining budget of `N` and prove
  that exactly one is rejected without overspend;
- retry tests never duplicate protected side effects;
- long-running and human-task workflows return and enforce authorized handles;
- revoking the initiating subject after acceptance denies status/result access,
  and result filtering never discloses more than the intersection of current
  authorization and the stored publication-time ceiling;
- cancellation reaches a terminal state or reports a stable non-cancellable
  reason; and
- rollback changes new starts without changing in-flight snapshots.

### Phase 3: AI-Assisted Authoring

Owners: `portal-view`, Portal GenAI services, and workflow validation.

- Generate drafts from selected registered operations and schemas.
- Generate contract, mapping, edge, and failure fixtures.
- Show assumptions, dependency graph, policy findings, and a human-readable
  diff.
- Record generator model, prompt/template version, source schema digests, and
  reviewer approval as provenance.
- Prohibit direct AI-to-production publication.

Implementation contract:

- `workflow-query` exposes `generateWfDefinitionDraft` as a draft-only query.
  It never creates, updates, or publishes a workflow.
- The query sends only explicitly selected, authorization-filtered tool
  metadata to an OpenAI-compatible authoring model. It strips secret-bearing
  fields, caps the operation count and context size, treats descriptions and
  schemas as untrusted data, and refuses credentials in the intent or existing
  definition.
- Configure the authoring service with `WORKFLOW_AUTHORING_LLM_URL` and
  `WORKFLOW_AUTHORING_LLM_MODEL`. `WORKFLOW_AUTHORING_LLM_BEARER_TOKEN` is
  optional, and `WORKFLOW_AUTHORING_LLM_TIMEOUT_SECONDS` is bounded to 1-60
  seconds. These values remain server-side and are never returned to Portal.
- The model response is accepted only as strict JSON containing definition,
  assumptions, policy findings, and contract, mapping, edge, and failure
  fixtures. A deterministic `workflow-mcp-phase3` validator then rejects
  unavailable tools, non-MCP generated calls, unsupported tasks, nested or
  unbounded forks, jq, JavaScript, and non-CEL expression profiles.
- Portal shows the proposal, bounded human-readable diff, assumptions,
  dependency graph, fixture categories, policy findings, and generator
  provenance. Applying it requires a signed-in reviewer checkbox and records
  model, prompt-template, source-schema, request, definition-digest, and
  reviewer evidence under `document.metadata.aiAuthoring`.
- Strict server validation binds `reviewerUserId` to the authenticated subject;
  it does not trust the reviewer identity supplied by the browser.
- The first save of an AI-authored draft is private. Server validation fails
  closed when the authorization-filtered catalog or validator is unavailable,
  and recomputes the semantic definition digest after removing only provenance
  metadata so post-approval edits require another review. Production exposure
  remains a separate promotion action using the normal gates.
- Workflow create and update command handlers independently reject attempts to
  set `catalogVisible: true` on an AI-authored definition, so direct RPC calls
  cannot bypass the private-first rule.

Exit gates:

- generated definitions cannot reference unavailable tools or unsupported DSL
  features;
- secrets and unauthorized catalog entries never enter generation context;
- deterministic validators reject unsafe or inconsistent drafts; and
- manual and AI-authored workflows pass the same promotion gates.

Run `scripts/run-workflow-mcp-phase3-gates.sh` from `light-fabric` to execute
the existing Phase 2 runtime gates plus the authoring-service tests, Portal
review tests, lint checks for the Phase 3 UI, and a release-mode Portal build.
This is an implementation check, not production qualification.

### Phase 4: Optional Skill Integration

Owners: Portal skill registry and agent catalog.

- `skill_workflow_t` may carry `workflow_binding_id` and the binding-derived
  `workflow_tool_id`. Both are nullable so ordinary skill/workflow links remain
  valid, but they must either both be absent or both be present.
- A composite foreign key binds the skill link to the exact
  `(host, binding, workflow, tool)` tuple. A second foreign key requires that
  exact tool to be present in `skill_tool_t`, so progressive disclosure cannot
  advertise a capability the skill was not granted.
- Portal lists only active workflow-backed bindings whose workflow matches the
  selected definition and whose current tool schema digest matches the pinned
  binding digest. The command service derives `workflow_tool_id` from the
  trusted binding; browsers cannot supply it.
- Query and effective-agent-catalog responses include the tool name, bound
  workflow version, definition digest, and schema digest. The Skill Workspace
  validates these invariants and renders the resolved contract for reviewers.
- Direct MCP discovery remains independent: `workflow_tool_binding_t` has no
  foreign key to a skill, and a published binding with no skill link continues
  to be projected and invoked normally.

Exit gates:

- a linked skill, tool, binding, and workflow resolve to one exact pinned
  contract;
- a mismatched workflow or a tool absent from `skill_tool_t` is rejected by
  database constraints and command validation;
- a workflow-backed MCP tool without a skill link remains valid and directly
  discoverable;
- create/update and validation RPC schemas expose the optional binding without
  accepting a caller-selected tool id; and
- Portal exposes the optional selector and the resolved version/digests.

Run `scripts/run-workflow-mcp-phase4-gates.sh` from `light-fabric` to execute
the prior runtime/authoring gates, the Portal database schema/constraint gate
when a disposable PostgreSQL URL is supplied, Portal persistence and GenAI
command/query tests, and the Portal UI lint/build checks. The development
contract does not include legacy-data migration or backward-compatibility
validation.

## Acceptance Criteria

The design is complete when:

- an existing MCP client can invoke a meaningful multi-API capability through
  one ordinary `tools/call`;
- no agent-side skill or orchestration implementation is required;
- the gateway contains no general workflow interpreter or executable user
  scripts;
- one canonical workflow definition backs both skill-aware and direct MCP
  exposure;
- every published tool is bound to stable tool, schema, workflow, dependency,
  and policy digests;
- synchronous result latency and tenant fairness are qualified against declared
  executor capacity, backlog, and crash-recovery gates;
- nested synchronous calls cannot be starved by root wait permits and inherit
  the outer execution class through signed delegation;
- synchronous and asynchronous behaviors are explicit in the tool contract;
- retries, cancellation, idempotency, and compensation have one runtime owner;
- outer and nested authorization remain tenant- and caller-bound;
- private version dispatch preserves the logical tool's authorization identity,
  and promotion or retirement cannot strand active outer dependencies;
- shared-principal asynchronous access is also bound to the initiating
  end-user subject, current authorization, and stored response-filter ceiling;
- AI-generated workflows are drafts until deterministic checks and human
  approval complete; and
- publication, promotion, retirement, and rollback preserve in-flight workflow
  snapshots and audit history.

## Settled And Remaining Decisions

This design settles the architecture choices that gate implementation:

- Phase 1 is read-only.
- Gateway starts use direct transactional durable acceptance rather than the
  shared event consumer.
- Nested calls select identity per step, defaulting to narrowed initiating-user
  delegation; workflow service identity requires explicit approval evidence.
- The workflow `output` block is the only executable public-result mapping.
- Workflow execution is selected by `executionPlacement`, not a new transport
  value in `apiType`.

The remaining product and operational choices require measured customer or
environment data rather than another runtime architecture:

1. What p95/p99 acceptance, execution, and wait SLOs should each deployment
   use, and what maximum synchronous wait follows from those measurements?
2. How many customer agents dynamically refresh `tools/list`, and how many use
   static allowlists that require an explicit rollout procedure?
3. Which asynchronous result fields remain queryable after the initiating MCP
   session ends, and what retention and legal-hold policies apply?

## Related Documentation

- [MCP Router](../../design/mcp-router.md)
- [MCP Tool Metadata Usage](mcp-tool-metadata-usage.md)
- [MCP Tools Access Control](mcp-tools-access-control.md)
- [MCP Tools List Access Control](mcp-tools-list-access-control.md)
- [Skill Workflow Orchestration](../../design/skill-workflow-orchestration.md)
- [Workflow Client Architecture](../../design/workflow-client-architecture.md)
- [Start Workflow](../light-workflow/start-workflow.md)
