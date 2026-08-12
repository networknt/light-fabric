# Workflow-Backed MCP Tools

Status: Proposed

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
- the current runtime expression evaluator supports a limited path,
  interpolation, literal, and comparison subset rather than general jq
  transformations; and
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
| `result_expression` | Approved public-result mapping when not declared in the workflow output. |
| `idempotency_policy` | Required key, derived business key, or read-only handling. |
| `delegation_policy` | Allowed nested tool references, audiences, and maximum depth. |
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

## Projected Gateway Configuration

The existing runtime uses `apiType` to select HTTP or MCP execution. The
minimal additive runtime change is `apiType: workflow`, while
`executionPlacement: workflow` remains the canonical catalog concept.

Example projected tool:

```yaml
- name: recommend_customer_offer
  description: Recommend and record the best eligible customer offer.
  method: call
  apiType: workflow
  endpoint: recommend_customer_offer@call
  inputSchema:
    type: object
    additionalProperties: false
    required:
      - customerId
      - channel
    properties:
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
    waitTimeoutMs: 20000
    totalDeadlineMs: 30000
    maximumSteps: 8
    maximumParallelism: 4
    maximumDelegationDepth: 1
  toolMetadata:
    routing:
      domain: Offers
      semanticNamespace: customer-offers
      semanticDescription: Recommend an eligible personalized customer offer.
      semanticKeywords:
        - recommend offer
        - personalized offer
        - customer eligibility
      sourceProtocol: workflow
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

The configuration contains only an approved reference, contract, and bounds.
It does not contain executable scripts or caller-selectable destinations.

The metadata ownership and compact `tools/list` rules in
[MCP Tool Metadata Usage](mcp-tool-metadata-usage.md) continue to apply.

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
Idempotency-Key: <approved-key>
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
    "customerId": "CUST-1001",
    "channel": "portal"
  }
}
```

Tenant and caller identity come from the authenticated delegation token. If a
tenant identifier is also carried in the body or transport metadata, it must
match the authenticated identity and fail closed on disagreement.

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

### Public Result

`light-workflow` must produce an explicit public result from the workflow
`output` definition or the approved binding result expression. The public
result is validated against the workflow output schema before the instance is
reported as `COMPLETED`.

The gateway then applies its current response-filter and output-validation
pipeline. A workflow must not return raw backend transport envelopes directly
to the agent.

## Gateway Execution Flow

For `tools/call`, the gateway performs:

1. Resolve the gateway-facing name to one immutable tool and workflow binding.
2. Apply tools-call authorization using the composite tool endpoint key.
3. Validate arguments against the published input schema.
4. Mask or transform arguments according to approved request policy.
5. Construct correlation, deadline, idempotency, and delegation context.
6. Start the workflow using the pinned definition version and digest.
7. Wait only when the tool is published as synchronous.
8. Map workflow state and public output into an MCP tool result.
9. Apply response filtering for the initiating caller.
10. Validate successful structured content against `outputSchema`.
11. Emit gateway and workflow correlation/audit attributes.

If the invocation service reports a different workflow digest, tenant, stable
tool reference, or schema binding, the gateway fails closed.

## Synchronous Tools

Synchronous tools preserve the simplest compatibility contract: an existing
agent issues one `tools/call` and receives the business result.

The initial synchronous profile should allow only bounded, headless workflows.
Recommended starting limits are:

| Limit | Initial value |
|-------|--------------:|
| Total tasks | 8 |
| Parallel branches | 4 |
| Nested workflow-tool depth | 1 |
| Gateway wait | 20 seconds |
| Total workflow deadline | 30 seconds |

These are starting defaults, not protocol constants. They should be
environment-configurable and publication-validated.

The first profile should allow deterministic API/MCP/rule calls, `set`,
`switch`, and `assert`. It should reject human `ask` tasks, unbounded model
calls, runner tasks, schedules, and unbounded loops.

When the gateway wait expires, it returns an MCP error result containing a
machine-readable workflow instance ID, state, and retryability. The workflow
may continue durably unless its published cancellation policy says otherwise.
The gateway must never silently start a second instance when the caller retries
with the same idempotency key.

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

Do not make one tool unpredictably return either a business result or an async
handle unless its published output schema explicitly models both outcomes.

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
schema digest
endpoint/policy reference
lifecycle status
```

Runtime calls are rejected if those bindings drift. A future internal
dispatch-by-stable-reference API can remove dependence on gateway-facing names,
but the public MCP name remains useful for compatibility and diagnostics.

## Delegation And Cycle Prevention

The gateway issues a short-lived workflow-task delegation token containing or
binding:

- tenant and initiating principal;
- outer stable tool reference;
- allowed nested stable tool references;
- allowed audiences and operations;
- input/data-boundary digest;
- correlation ID;
- deadline and cost/action budget;
- idempotency context; and
- remaining delegation depth.

Nested calls can only narrow these rights. They cannot extend the initiating
deadline, add tools, broaden the data boundary, or change tenant.

Portal publication builds a dependency graph for every workflow-backed tool.
It rejects:

- a workflow that calls its own composite tool;
- a cycle across two or more workflow-backed tools;
- a call to an unbound or retired tool;
- a nested call whose schema digest is unresolved; and
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

Read-only workflows can initially use an argument and caller digest for bounded
deduplication. Side-effecting workflows require a stronger business idempotency
contract.

The publication UI must require one of:

- an explicit idempotency input field;
- a deterministic business-key expression;
- an upstream server-enforced idempotency key; or
- a declaration that duplicate effects are impossible or compensated, with
  approval evidence.

The workflow invocation service stores the accepted idempotency key with the
stable tool reference, workflow digest, tenant, caller, and normalized input
digest. A duplicate request returns the original instance rather than creating
a second process.

For multi-step writes, the workflow definition owns compensation. The gateway
does not attempt to reverse completed backend operations.

## Failure Mapping

Keep business outcomes separate from technical failures.

Business outcomes such as `NO_CONSENT` or `NO_ELIGIBLE_OFFER` are successful,
schema-valid tool results. Technical failures produce `isError: true` and a
stable machine-readable class such as:

```text
WORKFLOW_START_REJECTED
WORKFLOW_DEFINITION_MISMATCH
WORKFLOW_TIMEOUT
WORKFLOW_CANCELLED
WORKFLOW_TASK_FAILED
WORKFLOW_OUTPUT_INVALID
WORKFLOW_POLICY_DENIED
WORKFLOW_CAPACITY_EXHAUSTED
```

The error envelope should include the workflow instance ID when one exists,
whether retry is safe, and a correlation ID. It must not expose credentials,
raw internal errors, hidden task inputs, or backend responses that have not
passed disclosure policy.

## Transformation And Aggregation Language

Use the workflow DSL as the only authoring contract. Do not invent a gateway
mapping language for composite tools.

The production transformation profile should provide one deterministic,
sandboxed jq-compatible expression engine for:

- selecting fields;
- reshaping objects and arrays;
- joining previously exported task results;
- computing derived values;
- filtering collections; and
- constructing the public output.

JavaScript should not be enabled merely because it appears in the workflow
model. The authoring validator must reject languages and jq features that the
runtime does not actually implement.

Parallel aggregation requires explicit fork/join semantics with bounded
parallelism and a deterministic merge rule. Step retries require explicit
attempt count, retryable error classes, backoff, jitter, and idempotency
requirements. These semantics belong in `light-workflow`, not in the MCP tool
configuration.

If declarative transformations are insufficient, an approved isolated runner
task may be used. Its input, output, image/template digest, resource limit, and
execution policy must be pinned. It cannot execute in the gateway process.

## Portal Authoring Experience

Add a **Composite MCP Tool** workspace to `portal-view`. Reuse the existing
workflow editor, validation, graph, and test-run surfaces.

### Contract

The user defines:

- MCP name, description, semantic metadata, and examples;
- input and output JSON Schemas;
- synchronous or asynchronous mode;
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

The UI previews the input and output shape at each step.

### Generate With AI

AI generation is a draft-authoring feature, not a production execution path.
The generation request contains:

- the user's business objective;
- only the APIs and MCP tools selected or authorized for the author;
- their schemas, descriptions, examples, and safety metadata;
- the supported workflow DSL and expression subset;
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
7. Safety, approval, idempotency, and data-boundary policy checks.
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
runtime bounds
test evidence
approver and publication metadata
```

The Portal projects the runtime subset into `mcp-router.tools`. The gateway
continues to use last-known-good configuration when a new snapshot is invalid.

Promotion atomically moves the tool alias to the new approved binding. New
calls use the new binding; in-flight workflows continue using their stored
definition and policy snapshots.

Rollback republishes the previous approved binding. It does not mutate or
delete historical definitions or running instances.

Retirement removes the tool from new `tools/list` responses and rejects new
starts while preserving status, result, cancellation, and audit access for
already accepted instances according to retention policy.

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
workflow.state
workflow.task_count
workflow.nested_call_count
workflow.delegation_depth
workflow.wait_ms
workflow.total_ms
```

Metrics should cover:

- starts, completions, failures, cancellations, and timeouts;
- synchronous wait latency and total workflow latency;
- active and waiting instances;
- duplicate/idempotent start hits;
- definition, schema, and policy mismatch rejections;
- nested-call denials and cycle/depth rejections;
- output-validation failures; and
- capacity rejection by tenant, tool, and workflow version.

Do not attach raw inputs, intermediate context, or final results to metrics.
Trace and audit payload capture follows classification and redaction policy.

## Capacity And Availability

The gateway must bound workflow dispatch independently from HTTP and MCP
backend dispatch. Recommended controls include:

- global and per-tenant concurrent workflow starts;
- per-tool concurrent synchronous waits;
- workflow-invocation connection and response timeouts;
- circuit health for the invocation service;
- request and public-result size limits;
- maximum pending asynchronous instances where policy requires it; and
- overload responses that distinguish safe retry from an accepted workflow.

A gateway timeout must not be reported as "not started" after the workflow
service has durably accepted the instance. The invocation service returns the
instance ID as part of durable acceptance, and retries use idempotency lookup to
resolve uncertain outcomes.

The gateway remains stateless with respect to workflow progress. Gateway
restart or reload does not lose the workflow instance.

## Implementation Phases

### Phase 0: Contract And Threat Model

Owners: `light-fabric`, `portal-db`, and `light-portal`.

- Define the invocation API, state model, public result, and error envelope.
- Define `workflow_tool_binding_t` and its command/query events.
- Define definition, schema, dependency, and policy digest rules.
- Define delegation claims, idempotency semantics, cancellation behavior, and
  synchronous eligibility rules.
- Publish OpenAPI/JSON Schema fixtures and positive/negative conformance tests.

Exit gates:

- the same normalized request has one canonical digest;
- stale definition or schema digests fail closed;
- cross-tenant start/status/result/cancel tests fail closed; and
- duplicate idempotency tests resolve to one workflow instance.

### Phase 1: Read-Only Synchronous MVP

Owners: `light-gateway`, `light-workflow`, `light-portal`, and `portal-view`.

- Add the workflow execution variant and configuration parser to the gateway.
- Add the stable invocation façade and bounded wait operation.
- Support headless, bounded sequential compositions.
- Add manual Composite MCP Tool authoring, validation, test, and publication.
- Use narrowed delegation for nested MCP calls.
- Restrict the first profile to read-only or otherwise provably idempotent
  operations.

Exit gates:

- an unchanged generic MCP client discovers and calls a composite tool;
- the final result passes gateway response filtering and output validation;
- gateway restart does not lose an accepted workflow;
- workflow/gateway traces share one correlation ID;
- recursive workflow-tool dependencies are rejected; and
- the configured step, payload, wait, and total-deadline bounds are enforced.

### Phase 2: Production Orchestration

Owners: `light-workflow` and the workflow client in `light-gateway`.

- Complete deterministic jq-compatible transformations.
- Add bounded fork/join aggregation and generic task retries.
- Add explicit task and workflow deadlines.
- Add asynchronous start/status/result/cancel tools.
- Add side-effect idempotency, approval, and compensation policies.
- Add dependency-drift checks and version promotion/rollback.

Exit gates:

- parallel partial failure is deterministic and auditable;
- retry tests never duplicate protected side effects;
- long-running and human-task workflows return and enforce authorized handles;
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

Exit gates:

- generated definitions cannot reference unavailable tools or unsupported DSL
  features;
- secrets and unauthorized catalog entries never enter generation context;
- deterministic validators reject unsafe or inconsistent drafts; and
- manual and AI-authored workflows pass the same promotion gates.

### Phase 4: Optional Skill Integration

Owners: Portal skill registry and agent catalog.

- Link workflow-backed tools to skills for richer guidance and progressive
  disclosure.
- Keep direct MCP discovery available for customers that do not use skills.
- Ensure skill and tool references resolve to the same workflow version and
  contract digest.

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
- synchronous and asynchronous behaviors are explicit in the tool contract;
- retries, cancellation, idempotency, and compensation have one runtime owner;
- outer and nested authorization remain tenant- and caller-bound;
- AI-generated workflows are drafts until deterministic checks and human
  approval complete; and
- publication, promotion, retirement, and rollback preserve in-flight workflow
  snapshots and audit history.

## Open Decisions

The following choices should be settled before Phase 1 implementation:

1. What percentage of composite tools must complete within the initial
   synchronous wait target?
2. Should Phase 1 permit any writes, or only read-only compositions?
3. Should nested calls normally use initiating-user delegation, a workflow
   service identity, or a per-step selectable policy?
4. How many customer agents dynamically refresh `tools/list`, and how many use
   static allowlists?
5. Which result fields must remain queryable after the initiating MCP session
   ends, and for how long?
6. Is the current event-driven workflow start latency acceptable for
   interactive tools, or does the invocation façade need a lower-latency
   accepted-start path?

## Related Documentation

- [MCP Router](../../design/mcp-router.md)
- [MCP Tool Metadata Usage](mcp-tool-metadata-usage.md)
- [MCP Tools Access Control](mcp-tools-access-control.md)
- [MCP Tools List Access Control](mcp-tools-list-access-control.md)
- [Skill Workflow Orchestration](../../design/skill-workflow-orchestration.md)
- [Workflow Client Architecture](../../design/workflow-client-architecture.md)
- [Start Workflow](../light-workflow/start-workflow.md)
