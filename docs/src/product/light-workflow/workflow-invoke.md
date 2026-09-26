# Workflow Invoke And Tool Binding Publication

Status: Design draft for issue #415; not implemented

This document defines two related changes to workflow-backed MCP Tools:

1. A new native `workflow_invoke` MCP operation on `light-workflow`. The
   Gateway calls it for every workflow-backed Tool. It admits the call against
   the Workflow-owned Tool binding, waits for completion within the binding's
   deadline, and returns the output or a clear timeout or failure result.
2. Publication of workflow definitions and Tool bindings to `light-workflow`
   through Workflow MCP operations, replacing the direct database sync.

The Gateway is the only caller of `workflow_invoke`. An Agent that needs a
workflow calls the workflow-backed MCP Tool on the Gateway like any other
Tool. A skill can guide the Agent to that Tool through `skill_tool_t`, but it
never calls the workflow directly.

It supersedes the **Workflow Start and Invocation API** section of
[Workflow-Backed MCP Tools](../light-gateway/workflow-backed-mcp-tools.md) for
workflow-backed Tools. `workflow_start` remains the entry point for editors,
schedulers and other asynchronous callers, as described in
[Start Workflow](start-workflow.md).

## Problem

The issue #415 checkpoint unified every root start on the native
`workflow_start` MCP tool. That is right for editor and scheduler starts, but
workflow-backed Tools lost the binding contract on the way:

- **Permanent idempotent replay.** The Gateway passes the derived idempotency
  key to `workflow_start`, which hard-codes `result_replay_until` to the
  maximum timestamp. A binding with `resultReplayMs: 0` (the normalizer
  default) now replays the first result forever, so a read-only Tool called
  twice with the same input by the same user never sees fresh data.
- **Binding runtime fields are dropped.** `workflow_start` hard-codes the async
  mode, the interactive class, a 30-day deadline, `permit_depth: 0` and no
  parent action. The binding's `totalDeadlineMs`, `executionClass`,
  `runtimeBounds` and `delegationPolicy` never reach admission, and a nested
  Tool call from a running workflow becomes an unlinked root.
- **Workflow-backed admission is skipped.** `workflow_start` admits with the
  `PortalExecution` profile and no broker, peer or policy. The
  `WorkflowBacked` checks (`verify_binding`, orchestration and pinned
  dependency validation, approval evidence, deadline-aware admission) and the
  dual-identity check do not run.
- **Sync waiting lives in the Gateway.** The Gateway loops on
  `workflow_wait_result`, which returns null for failed or cancelled runs, so
  a terminal failure is indistinguishable from a slow run until the Gateway
  deadline. Lifecycle tool errors are then collapsed to
  `WORKFLOW_INVOCATION_UNAVAILABLE`.
- **Binding data reaches Workflow by database access.** The development
  `workflow-projection-sync` Compose service runs
  `publish-workflow-projections.sh` every 30 seconds. It reads Portal tables
  over `postgres_fdw` and writes Workflow's `workflow_ops` schema directly.
  In production the Workflow runtime may belong to another team, and the
  Portal cannot access its operations database. The sync is also
  asynchronous to Gateway publication, so the Gateway can serve a binding that
  Workflow has not seen yet, or the reverse.

## Goals

- Workflow-backed Tools are admitted by Workflow from the Workflow-owned
  binding row, not from Gateway-supplied runtime fields.
- One MCP call returns the output, a terminal failure, or a timeout that
  identifies the running instance.
- Portal publishes definitions and bindings only through Workflow MCP
  operations, and receives a digest receipt it can pin in the Gateway
  configuration.
- The Gateway snapshot is promoted only after Workflow has accepted the
  matching binding, so the two sides cannot skew silently.

## Non-Goals

- Changing `workflow_start` semantics for editor, scheduler or API callers.
- Changing the workflow definition language or task execution.
- Agents calling `workflow_invoke` directly. They use the workflow-backed
  Tool, so there is one access-control and admission path.
- Running publication itself as a workflow. Publication records a pending
  binding and the owner decides separately (see **Workflow owner approval**).
- Backward compatibility with the projection sync. It is removed, not
  adapted.

## Tool Binding Model

A Tool binding connects one MCP Tool (`tool_t`) to one published workflow
definition version. The Portal creates it when a Tool is saved with
`executionPlacement=workflow` and a `workflowVersionRef`.
`WorkflowToolBindingNormalizer` fills defaults and rejects unsafe
combinations. The binding is stored in Portal `workflow_tool_binding_t`.

| Field | Meaning | Normalizer default |
|-------|---------|--------------------|
| `bindingId`, `toolId`, `toolName` | Binding identity and the stable Tool reference | from the Tool |
| `wfDefId`, `workflowVersion` | Pinned definition version | from `workflowVersionRef` |
| `definitionDigest`, `schemaDigest` | Hashes of the definition and its input schema | computed |
| `invocationMode` | Always `sync` for Tools (see **Sync only**) | `sync` |
| `syncWaitMs` | How long the caller waits for output | 20000, max 20000 for sync |
| `totalDeadlineMs` | Hard deadline for the run | 30000, max 30000 for sync |
| `executionClass` | Scheduling class | `interactive` for sync |
| `resultTextMode` | How output is rendered to MCP text | `compact-json` |
| `idempotencyPolicy` | Key kind and replay windows | kind `derived`, `resultReplayMs` 0 |
| `delegationPolicy` | Nested call rules | `maximumDelegationDepth` 1 |
| `runtimeBounds` | Attempts, nested calls, parallelism, bytes, cost units | 8, 8, 1, 1 MiB / 4 MiB / 1 MiB, 1000 |
| `admissionLimits` (new) | Concurrent runs and start rate, total and per user | see **Concurrency and rate limits** |
| `callerPolicy` (new) | Roles a user needs to reach the workflow through this Tool | empty: the Tool ACL decides |
| `policyDigest`, `responsePolicyDigest` | Hashes of the admission and response profiles | computed |

The database enforces `total_deadline_ms >= sync_wait_ms` and that sync mode
uses the interactive class.

Related records travel with the binding:

- **Dependencies** (`workflow_tool_dependency_t`): nested Tools that the
  workflow may call, with contract digest, authorization key and dispatch
  target.
- **Endpoint targets** (`workflow_endpoint_target_t`): the resolved HTTP
  endpoints that tasks may call, with allowed methods and authorization policy
  digest.
- **Grants** (`workflow_tool_grant_t`): LightAPI grants for the Tool version
  and the environments where they apply.
- **Approval evidence** (`workflow_tool_approval_evidence_t`): recorded
  approvals for tasks that need them.

The binding has two consumers:

- The **Gateway** needs just enough to list and route the Tool: name, input
  schema, the pins and digests, the mode, and the wait time for its HTTP
  timeout. It gets these from `mcp-router.yml`.
- **Workflow** needs the full binding to admit a call. It gets the binding
  from publication (below) and treats it as authoritative.

### Sync only

Every workflow-backed Tool is synchronous. The caller waits for the output
within one MCP call, and the whole run fits inside `totalDeadlineMs` (at most
30 seconds). The normalizer rejects `invocationMode: async`, and the async
Tool behaviour in
[Workflow-Backed MCP Tools](../light-gateway/workflow-backed-mcp-tools.md)
is out of scope. A long-running or human-in-the-loop process is started with
`workflow_start` instead.

### Read-only and write Tools

Workflows that update data or drive a business process can be Tools. Today
the normalizer rejects a sync binding unless the Tool is marked `readOnly`,
not `destructive` and not `humanApprovalRequired`, which means write
workflows could only be async Tools.

The read-only rule exists because a sync caller can give up before the run
finishes. An MCP client or Agent that times out usually retries. With a
read, a duplicate run costs capacity. With a write, the client cannot tell
whether the first attempt changed anything, and the retry may do it twice.

With `workflow_invoke` the retry re-attaches to the in-flight run, which
removes most of that risk. Recommended rules:

| Tool | Allowed | Required |
|------|---------|----------|
| Read-only | yes | nothing extra |
| Writes | yes | idempotency kind `explicit` or `business`, or `derived` with `resultReplayMs` of at least the client retry horizon (for example 10 minutes); `readOnly: false` so clients see the write hint |
| Destructive | yes | as above, plus the Tool's `destructive` flag so clients ask for confirmation |
| Contains a human task | no | a human step cannot finish inside the deadline; use `workflow_start` |

Write tasks inside any bound workflow still need their publication approval
evidence, as today. The normalizer change is to replace "sync must be
read-only" with the table above.

### Concurrency and rate limits

`runtimeBounds` limits one run. `admissionLimits` limits how many runs the
Tool can start, so the workflow owner can approve a known load:

```json
{
  "maximumConcurrentRuns": 20,
  "maximumConcurrentRunsPerUser": 2,
  "startsPerMinute": 120,
  "startsPerMinutePerUser": 10
}
```

The normalizer fills these defaults when the Tool author leaves them out,
the same way it fills `runtimeBounds`. The author can change them, and the
workflow owner approves the final values.

The four numbers must agree with each other. For a sync Tool, the number of
runs in flight is roughly the start rate times the average run time. At 120
starts per minute (2 per second) and a typical 5 second run, about 10 runs
are in flight, well under 20. The concurrent cap only bites when runs slow
down toward the 30 second deadline: 2 per second × 30 seconds would be 60,
and the cap holds it at 20. That is the case it is for, since a slow
downstream API should not have more and more runs piling onto it. The
per-user values (2 concurrent, 10 per minute) stop one user or a looping
Agent from taking the whole allowance.

Workflow enforces the limits at
admission, counting runs of this binding that are not yet terminal and starts
within a sliding one-minute window. A re-attach to an in-flight run does not
count as a new start. When a limit is hit, Workflow returns
`WORKFLOW_CAPACITY_EXHAUSTED` with `retryAfterMs`. The Gateway permit pool
still protects the Gateway itself, but Workflow's limits are the ones the
owner approves.

## Publication Through Workflow MCP

### Operations

Workflow exposes these operations on its native MCP endpoint, next to
`workflow_start`. They are declared in
`contracts/workflow-admin/tool-manifest.json` with schemas and examples like
the other workflow-admin tools.

| Tool | Permission | Purpose |
|------|------------|---------|
| `workflow_definition_publish` | `workflow.definition.publish` | Publish one immutable definition version |
| `workflow_definition_retire` | `workflow.definition.publish` | Stop new admissions to a definition version |
| `workflow_binding_publish` | `workflow.binding.publish` | Publish a Tool binding with its dependencies, endpoint targets, grants and approval evidence |
| `workflow_binding_retire` | `workflow.binding.publish` | Deactivate a Tool binding, called by the Tool owner |
| `workflow_binding_get` | `workflow.binding.read` | Read one binding with its status and approval history |
| `workflow_binding_list` | `workflow.binding.read` | List bindings for definitions the caller owns, filterable by status |
| `workflow_binding_decide` | `workflow.binding.approve` | Approve or reject a pending binding, called by the definition owner |
| `workflow_binding_revoke` | `workflow.binding.approve` | Withdraw approval of an active binding, called by the definition owner |

The Portal calls these tools on behalf of the user who clicked the action. It
forwards that user's token and adds the Portal app token in `x-scope-token`,
as `StartWorkflow` does. Workflow therefore knows who is publishing, which the
approval rules need. The Workflow owner decides which Portal clients may
publish for which host and namespace.

The Portal reaches these tools through the same Gateway `/mcp` route that
`StartWorkflow` uses, and the Gateway connects to Workflow. The Portal never
connects to Workflow or to the Workflow database directly. Today the Gateway
uses one configured Workflow endpoint. When each host runs its own
`light-workflow` instance, the Gateway will locate it through controller-rs
service discovery; the publication contract does not change.

### Definition publish

Input:

```json
{
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "wfDefId": "0198a3c2-...",
  "namespace": "claims",
  "name": "summarize-claim",
  "version": "1.2.0",
  "definition": { "document": { "dsl": "1.0.0", "...": "..." } },
  "expectedDefinitionDigest": "sha256:...",
  "owner": { "userId": "...", "positionId": "..." }
}
```

Workflow must:

1. Parse the definition and recompute its definition and input schema
   digests. It rejects the call if the result differs from
   `expectedDefinitionDigest`.
2. Run the same definition validation it runs at admission.
3. Treat `(hostId, wfDefId, version)` as immutable. Republishing the same
   digest returns the existing receipt with `result: "unchanged"`. A
   different digest for a published version is rejected with
   `WORKFLOW_DEFINITION_MISMATCH`.

Receipt:

```json
{
  "result": "published",
  "wfDefId": "0198a3c2-...",
  "version": "1.2.0",
  "definitionDigest": "sha256:...",
  "schemaDigest": "sha256:...",
  "aggregateVersion": 1
}
```

### Binding publish

The input carries the normalized binding and every related record Workflow
needs. The Portal resolves endpoint targets and grants from `tool_t`, `api_*`
and the permission tables before the call, which the projection script used
to do over the FDW. Workflow receives only resolved values.

```json
{
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "binding": {
    "bindingId": "...",
    "toolId": "...",
    "toolName": "claims.summarize",
    "wfDefId": "...",
    "workflowVersion": "1.2.0",
    "definitionDigest": "sha256:...",
    "schemaDigest": "sha256:...",
    "invocationMode": "sync",
    "syncWaitMs": 20000,
    "totalDeadlineMs": 30000,
    "executionClass": "interactive",
    "resultTextMode": "compact-json",
    "cancellationPolicy": "before-effects-only",
    "idempotencyPolicy": { "kind": "derived", "resultReplayMs": 0 },
    "delegationPolicy": { "maximumDelegationDepth": 1 },
    "runtimeBounds": { "maximumTaskAttempts": 8, "...": "..." },
    "policyDigest": "sha256:...",
    "responsePolicyDigest": "sha256:..."
  },
  "dependencies": [],
  "endpointTargets": [],
  "grants": [],
  "approvalEvidence": [],
  "expectedAggregateVersion": 3
}
```

Workflow must:

1. Require that the pinned definition version is already published and that
   its digests match.
2. Validate the binding fields and constraints, including the sync rules.
3. Run `validate_orchestration_definition` and `validate_pinned_dependencies`
   against the payload.
4. Check each endpoint target against Workflow's own destination policy. The
   Workflow owner can refuse a target that the Portal has resolved.
5. Decide the binding status (see **Workflow owner approval**).
6. Compute a `bindingDigest` over the canonical binding and its related
   records.
7. Upsert the binding and replace its related records in one transaction. Use
   `expectedAggregateVersion` for optimistic concurrency, with the same
   semantics as other aggregate writes.

Receipt:

```json
{
  "result": "published",
  "status": "active",
  "bindingId": "...",
  "toolId": "...",
  "workflowVersion": "1.2.0",
  "definitionDigest": "sha256:...",
  "bindingDigest": "sha256:...",
  "aggregateVersion": 4
}
```

`status` is `active` or `pendingApproval`. Republishing an identical payload
returns `result: "unchanged"` with the same digest and current status, so a
retry of the Portal publish is safe.

### Publish ordering

The Portal "Publish Selected" action runs these steps in order:

1. `workflow_definition_publish` for each pinned definition version that is
   not yet published.
2. `workflow_binding_publish` for each workflow-backed Tool in the selection.
3. Write `mcp-router.yml` with the `bindingDigest` and `definitionDigest`
   from the receipts of `active` bindings, and promote the Gateway snapshot.
   A `pendingApproval` binding is left out; if an earlier approved binding
   for the same Tool is still active, the snapshot keeps that one.

If step 1 or 2 fails, the Gateway snapshot is not promoted and the Portal
shows the Workflow error. The Gateway therefore never routes to a binding
that Workflow has not accepted.

Retirement runs in the reverse order. The Portal removes the Tool from the
Gateway snapshot first, then calls `workflow_binding_retire`. An admitted run
uses only the digests pinned at admission and never re-reads the binding row,
so retirement does not change runs in flight; they finish. New
`workflow_invoke` calls for the retired binding are rejected.

### Workflow owner approval

Binding a Tool to a workflow lets a new audience start that workflow, with a
deadline and budget the Tool author chooses. The workflow owner is
accountable for that load and for what the workflow does, so the owner must
approve a binding created by someone else. This is the reverse of the
existing Tool access approval:

| Direction | Requester | Approver | Mechanism |
|-----------|-----------|----------|-----------|
| A workflow calls a Tool | Workflow author | Tool owner | `RequestWorkflowToolAccess` → approval workflow → `DecideWorkflowToolAccess` → `workflow_tool_grant_t` |
| A Tool invokes a workflow | Tool author | Workflow owner | `workflow_binding_publish` → pending binding in Workflow → `workflow_binding_decide` |

The pending binding and the decision are held by Workflow, not the Portal.
Workflow belongs to the owner's team, and the owner's decision reaches it
with the owner's own user token, so Workflow does not have to trust the
Portal's word that approval happened.

#### Status rules

`workflow_binding_publish` sets the status:

- **active** when the publishing user is the definition owner (the recorded
  `owner_user_id`, or a holder of `owner_position_id`), or when the binding's
  approval digest matches an existing approval for the same Tool and the
  approval carries over (see **New definition versions**).
- **pendingApproval** otherwise. The binding is stored but inactive. An
  earlier active binding for the same Tool keeps serving until the new one is
  approved.

The **approval digest** covers the fields that change load, reach or
behaviour: mode, `syncWaitMs`, `totalDeadlineMs`, execution class,
cancellation, idempotency and replay, delegation, runtime bounds, admission
limits, caller policy, dependencies and endpoint targets. Changing any of
them needs new approval. Changing the Tool name, description or input
examples does not.

#### New definition versions

The definition version is not part of the approval digest. When the Tool
author re-pins to a newer version of the same definition, the approval
carries over if all of these hold:

- the binding fields in the approval digest are unchanged;
- the new version's dependencies and endpoint targets are the same as, or a
  subset of, the approved ones;
- the new version adds no write task that the approved version did not have;
- the new version fits the approved limits (below); and
- the owner did not mark the new version `bindingApproval: reapprove` when
  publishing it.

Otherwise the binding is `pendingApproval`.

A heavier new version cannot overload the workflow under a carried-over
approval, because the approved limits still apply to it: it fails with
`WORKFLOW_BUDGET_EXHAUSTED` or `WORKFLOW_TIMEOUT` instead of using more.
To stop that from surfacing as runtime failures, `workflow_binding_publish`
checks the new version's static requirements against the binding: fork
width against `maximumParallelism`, declared nested calls against
`maximumNestedCalls`, and task count against `maximumTaskAttempts`. A
version that cannot fit is rejected with a message that names the limit.
The author raises that limit, which changes the approval digest and sends
the binding to the owner.

The owner knows best when a version is heavier, so
`workflow_definition_publish` takes an optional `bindingApproval` of
`carryOver` (default) or `reapprove`. With `reapprove`, every binding that
re-pins to that version goes back to the owner.

Other statuses: **rejected** (with the owner's comment), **revoked** (the
owner withdrew an active approval) and **retired** (the Tool owner removed
it).

`workflow_binding_decide` input:

```json
{
  "bindingId": "...",
  "expectedBindingDigest": "sha256:...",
  "decision": "approve",
  "comment": "Approved for claims read-only lookups"
}
```

Workflow checks that the caller is the definition owner and that
`expectedBindingDigest` is the pending binding's digest, so the owner
approves exactly what they reviewed. On approval it records the evidence,
activates the binding and deactivates the previous binding for the Tool. A
rejection requires a comment.

`workflow_binding_revoke` deactivates an active binding immediately. New
`workflow_invoke` calls for that Tool return `WORKFLOW_POLICY_DENIED` with
"binding revoked by workflow owner" until the Portal removes the Tool from
the Gateway. Running instances finish.

#### Evidence storage

`workflow_tool_approval_evidence_t` is keyed by binding and currently holds
the approval evidence for write tasks inside a bound workflow (non-GET HTTP
calls and non-read-only MCP calls, checked by `validate_approval_evidence`).
Owner approval of a binding fits the same table with these changes:

- add `evidence_kind` (`task` or `binding`) and include it in the primary key;
- binding rows use an empty `task_name`, enforced by a check constraint;
- for binding rows, `evidence_digest` is the approval digest;
- add `decision` and `comment`, so rejections and revocations are recorded
  alongside approvals.

#### UI flow

All screens are in portal-view. Portal commands call the Workflow tools
through the Gateway with the acting user's token, and store the returned
status on the Portal `workflow_tool_binding_t` row so lists do not need a
Workflow call. The review form reads the live binding from Workflow.

**Tool author, GenAPI Admin → Tool**

1. The author creates or edits a Tool with `executionPlacement=workflow` and
   picks a published workflow version. The form shows the definition owner.
   When the author is not the owner it shows "This binding needs approval
   from &lt;owner&gt; before it can be served."
2. The author runs "Publish Selected". The result dialog lists each Tool:
   "Published", or "Waiting for approval from &lt;owner&gt;" for pending
   bindings. The rest of the batch is published normally.
3. The Tool list shows a binding status column: Active, Pending approval,
   Rejected, Revoked. Rejected and Revoked show the owner's comment. The
   author can edit and publish again, which creates a new pending binding.
4. After approval the status becomes "Approved, not yet on Gateway" until
   the next "Publish Selected" includes it. Gateway publication stays an
   explicit action, as it is today.

**Workflow owner, Workflow → Definitions**

1. The definition list shows a "Tool bindings" column with a badge for the
   pending count, for example "2 pending". The owner can filter the list to
   definitions with pending bindings.
2. The row action "Tool Bindings" opens a list of bindings for all versions
   of that definition: Tool name, Tool owner, definition version, mode,
   status, requested or decided time and who decided.
3. Selecting a pending binding opens the review form, loaded with
   `workflow_binding_get`:
   - **Requester:** Tool name, description, Tool owner, host.
   - **Target:** definition name, version and digest.
   - **Execution:** mode, sync wait, total deadline, execution class,
     cancellation policy.
   - **Limits:** idempotency and replay window, delegation depth, task
     attempts, nested calls, parallelism, request, intermediate and result
     bytes, cost units.
   - **Reach:** dependencies and endpoint targets.
   - **Change:** when an approved binding already exists for the Tool, the
     changed fields are highlighted against it.
   - A comment box and **Approve** / **Reject** buttons. Reject requires a
     comment.
4. An active binding has a **Revoke** action with a required reason.

**Worklist.** A pending binding also appears in the owner's Worklist, like
Tool access requests do today. Opening it goes to the same review form.
### What is removed

- The `workflow-projection-sync` service in `light-portal-install`
  `docker-compose.yml`.
- `crates/workflow-store/deployment/publish-workflow-projections.sh`, and the
  `postgres_fdw` server and user mapping it needs.
- Any documentation step that tells operators to run the sync.

The Workflow tables keep their current shape. Only the writer changes: the
publication handlers write them instead of the FDW script.

## Workflow Invoke

### Callers and routing

| Caller | Operation | Admission profile |
|--------|-----------|-------------------|
| Workflow Editor, scheduler, API client | `workflow_start` | `PortalExecution` |
| Gateway, executing a workflow-backed Tool for any MCP client, including Agents | `workflow_invoke` | `WorkflowBacked` |

### Identity and user token

`workflow_invoke` receives the same two credentials as `workflow_start`: the
original user token in `authorization`, and the Gateway app token in
`x-scope-token`. Workflow authenticates both through `dual_identity`.

Unlike `workflow_start`, there is no light-oauth registration and no token
exchange. The run uses the original user token for its outbound calls, and
the Gateway forwards the same token to nested `workflow_invoke` calls.

**No expiry check is needed.** Two settings keep the token valid for a run:

- The Gateway's stateless auth renews a user token before it expires
  (`renew_before_seconds`, default 90 in `spa_auth`; configurable).
- JWT verification accepts a token for `clock_skew_in_seconds` after its
  `exp` (default 60 in `light-security`; set in `security.yml`).

A run lasts at most `totalDeadlineMs` (30 seconds), which is less than the
60 second skew. So a token that verifies at admission still verifies at
every downstream call in the run. This holds as long as
`clock_skew_in_seconds` stays above the 30 second sync deadline cap; lowering
the skew below it would need an admission check again.

### Run credential selection

This rule applies to every run, including async runs started with
`workflow_start`. For each outbound HTTP or MCP call, the task executor picks
the user token like this:

1. Use the original user token if it is not about to expire (more than a
   configurable margin, for example 60 seconds, before its `exp`).
2. Otherwise, if the run has a LONG registration, use the token exchanged
   through light-oauth (`LongAuthority::token_for`).
3. Otherwise fail the call as an authentication failure. For a sync run this
   cannot happen within the deadline (see above).

Only `workflow_start` runs register with LONG, because only they can outlive
the original token. A `workflow_invoke` run never does.

This changes today's behaviour in two places in `bound_mcp.rs`:

- A LONG-registered run currently exchanges on every call
  (`token_for` always calls `exchange`), even while the original token is
  valid. It should use the original first.
- A run without LONG falls back to the grant broker
  (`CredentialBroker::renew_for_run`). The grant broker is to be removed, so
  that fallback goes away and step 3 applies.

**Where the original token is kept.** A run can move to another Workflow
replica, so the token must be stored, not only held in memory.

- `workflow_start` runs already store it encrypted in
  `workflow_long_credential_t` as part of the LONG registration.
- `workflow_invoke` runs store it encrypted with the same key handling, in a
  run credential row that expires at `deadline_ts`. It is deleted when the
  run reaches a terminal state, and a sweeper deletes any that outlive their
  expiry.

**Action authority without a grant.** The permit ledger that links nested
calls (`workflow_action_authority_t`, `workflow_action::Binding`) requires a
non-nil `grant_id`, which currently comes from the LONG registration. For
`workflow_invoke` runs the authority row is written at admission with
`grant_id` set to the run ID (or the column made nullable for sync runs),
the run's `deadline_ts` as its deadline, and an action limit from
`runtimeBounds`. The permit checks that compare grant generations work
unchanged.

### Authorization

A user calling a workflow-backed Tool needs permission for the Tool, not the
`workflow-admin` role.

`workflow_start` is an administration tool: it can start any saved
definition, so its Gateway ACL is limited to `admin`, `host-admin` and
`workflow-admin`. A workflow-backed Tool exposes one approved workflow
version with fixed limits. The Tool is the capability being granted, and its
ACL (`admin`, `host-admin`, `genai-admin`, or whatever the Tool owner sets)
decides who may call it. Requiring `workflow-admin` as well would give every
Tool user the power to start any workflow, which is the opposite of least
privilege.

`workflow_invoke` is therefore not ACL-checked against user roles at the
Gateway, because no client calls it. Workflow authorizes it in three layers:

1. **Caller.** The app token must belong to a Gateway client registered for
   this Workflow instance, with the `workflow.instance.invoke` scope.
2. **Binding.** The binding must be active and approved by the workflow owner.
3. **Caller policy.** If the binding has a `callerPolicy`, the user token must
   satisfy it, for example `{"anyRole": ["claims-agent", "genai-admin"]}`.

The caller policy exists because the Tool ACL lives in the Portal and the
Tool owner can widen it later without new approval. The policy is part of the
approval digest, so the workflow owner can pin who may reach the workflow,
and Workflow enforces it on every call. An empty policy means the owner
accepts the Tool ACL. The review form shows the Tool's current ACL next to
the caller policy.

Permission: `workflow.instance.invoke`. Only the Gateway may call it. The
request carries the user `authorization` and the Gateway app token in
`x-scope-token`, and Workflow runs the dual-identity check with the real
broker, peer and policy instead of the `None` values that `workflow_start`
passes. The Gateway does not expose `workflow_invoke` in `tools/list` and
refuses it as a direct `tools/call` from a client.

### Input

```json
{
  "stableToolRef": "0198a3c2-...",
  "expectedBindingDigest": "sha256:...",
  "expectedDefinitionDigest": "sha256:...",
  "input": { "claimId": "C-1001" },
  "parentActionId": "optional; only for a nested call from a running workflow"
}
```

The Gateway sends no deadline, budget, class, depth or idempotency key.
Workflow derives them from the binding.

### Trust split

| Value | Source |
|-------|--------|
| Tool identity, input | Gateway arguments, after the Gateway ACL and schema checks |
| Binding digest, definition digest | Gateway arguments, used only as pins that must match |
| Parent action | Gateway argument, verified by Workflow (see **Parent action**) |
| Caller identity | User token and Gateway scope token, verified by Workflow |
| Mode, wait, deadline, class, budget, delegation, idempotency, cancellation | Workflow binding row |

### Admission

Workflow must:

1. Load the active binding for `stableToolRef` and the caller's host.
2. Compare `expectedBindingDigest` and `expectedDefinitionDigest` to the row.
   On mismatch, return `WORKFLOW_DEFINITION_MISMATCH`. This detects a Gateway
   snapshot that is ahead of or behind Workflow.
3. Build the `StartInvocationRequest` from the row:
   - `mode` from `invocation_mode`, `execution_class` from the row
   - `deadline_ts` = min(now + `total_deadline_ms`, parent deadline)
   - `budget` from `runtime_bounds`, reduced to the parent's remaining budget
     for nested calls
   - `permit_depth` = parent depth + 1, rejected above
     `maximumDelegationDepth`
   - `cancellation_policy` from the row
   - `idempotency` as described below
4. Check `callerPolicy` and `admissionLimits`.
5. Admit through `start_invocation_with_stage` with `AdmissionProfile::WorkflowBacked`,
   so `verify_binding`, orchestration and dependency validation, approval
   evidence and `enforce_deadline_aware_admission` all run.

### Idempotency

For `kind: derived`, Workflow computes the key. The Gateway no longer does.

```text
scoped_key_digest = sha256(hostId, toolId, workflowVersion,
                           user subject, normalized_input_digest)
in_flight_until   = deadline_ts
result_replay_until = completion time + resultReplayMs
```

With the default `resultReplayMs: 0`, an identical call while the first run
is in flight attaches to that run, and a call after completion starts a new
run. A binding that sets a positive `resultReplayMs` gets replay for exactly
that window. A different input with the same key is rejected with
`WORKFLOW_IDEMPOTENCY_CONFLICT`.

The key uses the user subject only, not the Gateway client. A Workflow
instance sits behind one Gateway, so the subject is unique within it. The key
is there to stop one user from starting duplicate runs of the same Tool with
the same input, and the subject is enough for that.

### Waiting, timeout and re-attach

Workflow waits for the admitted run for
`min(sync_wait_ms, deadline_ts - now)`. The Gateway sets its HTTP timeout to
the binding's `syncWaitMs` from `mcp-router.yml` plus a 2 second margin, and
treats a transport timeout as `WORKFLOW_INVOCATION_UNAVAILABLE`.

When the wait ends without output:

- If the run can still finish before `deadline_ts`, Workflow returns
  `WORKFLOW_TIMEOUT` with the `workflowInstanceId` and `retryable: true`.
  The run continues. A retry with the same input maps to the same derived key
  and re-attaches to the running instance instead of starting another one.
- If `deadline_ts` has passed, Workflow cancels the run according to the
  binding's cancellation policy and returns `WORKFLOW_TIMEOUT` with
  `retryable: false`.

Sync bindings are read-only, so the default `before-effects-only` policy is
safe. Workflow does not cancel a run merely because the caller stopped
waiting.

`MAX_WAIT_MS` stays at 20000, matching the sync binding limit.

### Output

Success:

```json
{
  "status": "completed",
  "workflowInstanceId": "...",
  "definitionDigest": "sha256:...",
  "output": { "summary": "..." }
}
```

Failure, returned as an MCP `isError` result with `structuredContent`:

```json
{
  "status": "failed",
  "workflowInstanceId": "...",
  "error": {
    "code": "WORKFLOW_TASK_FAILED",
    "message": "task fetchClaim failed",
    "retryable": false,
    "afterEffect": false
  }
}
```

`error` is the run's stored normalized error, not a generic message.

### Error mapping

| Condition | Code | Retryable |
|-----------|------|-----------|
| Input fails the definition schema | `WORKFLOW_INPUT_INVALID` | no |
| No active binding, or admission refused | `WORKFLOW_START_REJECTED` | no |
| Binding or definition digest mismatch | `WORKFLOW_DEFINITION_MISMATCH` | no, until the snapshots converge |
| Policy or dual-identity check refused | `WORKFLOW_POLICY_DENIED` | no |
| Interactive capacity full, or a binding concurrency or rate limit hit | `WORKFLOW_CAPACITY_EXHAUSTED`, with `retryAfterMs` | yes |
| User fails the binding's caller policy | `WORKFLOW_POLICY_DENIED` | no |
| Same key, different input | `WORKFLOW_IDEMPOTENCY_CONFLICT` | no |
| Wait ended, run still within deadline | `WORKFLOW_TIMEOUT` | yes, re-attaches |
| Deadline passed | `WORKFLOW_TIMEOUT` | no |
| Run cancelled | `WORKFLOW_CANCELLED` | no |
| Task failed | `WORKFLOW_TASK_FAILED` | per stored error |
| Output fails the response contract | `WORKFLOW_OUTPUT_INVALID` or `..._AFTER_EFFECT` | no |
| Budget exhausted | `WORKFLOW_BUDGET_EXHAUSTED` or `..._AFTER_EFFECT` | no |
| Workflow unreachable (Gateway only) | `WORKFLOW_INVOCATION_UNAVAILABLE` | yes |

The Gateway passes Workflow's code and message through to the MCP client. It
uses `WORKFLOW_INVOCATION_UNAVAILABLE` only for transport failures, and the
same rule applies to `workflow_get_status`, `workflow_get_result` and
`workflow_cancel`.

### Parent action

An action is one outbound Tool call made by a running workflow task. The
parent action reference is the ID of that call. It links a child workflow run
to the task that caused it, so the child inherits the parent's authority
instead of starting as an unrelated root.

The flow already exists in the code:

1. **Workflow records the action.** Before a task calls a Tool
   (`bound_mcp.rs`), Workflow installs a permit in
   `workflow_action_permit_t`. The permit records the run, user, grant,
   Tool, contract digest, depth, maximum depth, execution class, deadline and
   budget limits (`workflow_action::Binding`).
2. **The task calls the Gateway.** The request carries the user token, the
   Workflow app token in `x-scope-token`, and `x-workflow-action: <action_id>`.
3. **The Gateway inspects the action.** `dual_identity` reads the header and
   `action_gateway.rs` asks Workflow for the stored permit over the mTLS
   action dispatch. The Gateway rejects the call if the permit's contract
   digest does not match the Tool, and uses the permit's depth and class for
   its own limits.
4. **The Tool is workflow-backed.** The Gateway calls `workflow_invoke` with
   `parentActionId = action_id` and forwards `x-workflow-action`.
5. **Workflow verifies the parent** (`rule_api.rs`, `receiver_parent`). It
   accepts the child only if:
   - the caller is the Gateway and its `x-workflow-action` equals
     `parentActionId`,
   - the Gateway's mTLS peer is a registered Gateway owner,
   - the permit is still active,
   - the user, claims digest and Tool match the permit, and
   - the child's depth is the parent's depth plus one, is within the maximum
     depth, uses the same class, and ends no later than the parent's deadline.

With `workflow_invoke`, Workflow derives the child's depth, class, deadline
and budget from the parent permit and the child binding, so the Gateway no
longer computes them. The child's deadline is the earlier of its own and the
parent's. If any check fails, Workflow returns `WORKFLOW_POLICY_DENIED`. It
never downgrades the call to a root start.

`parentActionId` comes only from a verified `x-workflow-action` permit.
An Agent calling a workflow-backed Tool is a root call unless it is itself
running as a workflow task with a permit.

The purpose is containment. Without the link, a workflow could call a Tool
that starts another workflow, which calls a Tool that starts another, each
with a fresh 30-second deadline and a full budget. The parent link caps the
depth, keeps the whole tree inside the first deadline and budget, and ties
every run to the original user for audit and cancellation.

Step 3 needs the mTLS workflow action dispatch in the Gateway. The #415
checkpoint stopped wiring it in production (`load_mcp_router_runtime`), so
nested calls fail closed until it is restored. The dispatch must be wired
again before nested workflow-backed Tools can work.

## Gateway Changes

- `execute_workflow_tool` calls `workflow_invoke` instead of `workflow_start`
  followed by `wait_for_workflow_start_result`. The wait loop is removed.
- `McpWorkflowBindingConfig` shrinks to what the Gateway uses: Tool name,
  `stableToolRef`, input schema, `workflowDefinitionId`, `workflowVersion`,
  `definitionDigest`, `bindingDigest`, `invocationMode`, `syncWaitMs` and
  `resultTextMode`. The Gateway keeps the request-size pre-check from
  `runtimeBounds.maximumRequestBytes` as an early rejection; Workflow still
  enforces the full budget.
- The Gateway no longer computes the idempotency key.
- Lifecycle tool errors pass through as described in **Error mapping**.
- `loadRuntimeWorkflowTool` in `ConfigPersistenceImpl` takes the digests from
  the Workflow receipts instead of recomputing them.

## Other Contract Changes

- **`cancellation_policy` column.** Add it to Portal and Workflow
  `workflow_tool_binding_t`, defaulting to `before-effects-only`, and to the
  normalizer. Today the binding has no way to express it.
- **`workflow_wait_result`.** Remove it from the Gateway path. If the UI needs
  a long-poll for `workflow_start` runs, keep it for those callers, but it
  must return the stored normalized error for failed and cancelled runs
  instead of null. Either way, add its `examples.json` entry so
  `validate-contracts.mjs` passes.
- **`workflow_start`.** Its semantics do not change. It stays async with the
  `PortalExecution` profile. It must reject a caller-supplied
  `stableToolRef`, so a Tool call cannot bypass binding admission through it.

## Implementation Plan

Each step stays on the issue branches (light-fabric #415, light-portal #842,
portal-view #919) until the end-to-end check passes.

1. **Workflow publication operations.** Manifest, schemas and examples for
   the five publication tools; handlers that validate and write the existing
   tables; the `cancellation_policy` column; the `bindingDigest`
   computation.
2. **Workflow owner approval.** `admissionLimits`, `callerPolicy` and the
   relaxed read-only rule in the normalizer and both binding tables; binding
   status, version carry-over, `workflow_binding_list`,
   `workflow_binding_decide` and `workflow_binding_revoke`, the evidence table
   changes, Portal commands that forward them, and the portal-view screens and
   Worklist entry.
3. **Portal publisher.** Resolve endpoint targets and grants in Java, call the
   publication tools in order, pin the receipts in `mcp-router.yml`, and
   block Gateway promotion on failure. Remove `workflow-projection-sync` and
   `publish-workflow-projections.sh`.
4. **`workflow_invoke`.** Manifest entry and handler built on
   `start_invocation_with_stage` with `WorkflowBacked`, binding-derived
   request fields, the run-scoped user token and grant-less action
   authority, caller policy and admission limits, wait, timeout and cancel, and the error
   result.
5. **Run credential selection.** Original token first, exchanged token near
   expiry, for both `workflow_start` and `workflow_invoke` runs; remove the
   grant broker fallback from `bound_mcp.rs` and the broker itself once
   nothing else uses it.
6. **Gateway.** Route to `workflow_invoke`, shrink the binding config, pass
   errors through, remove the wait loop and key derivation, keep
   `workflow_invoke` out of client-visible tools, and restore the mTLS action
   dispatch.
7. **Cleanup.** Resolve `workflow_wait_result`, and update
   [Workflow-Backed MCP Tools](../light-gateway/workflow-backed-mcp-tools.md)
   and [Start Workflow](start-workflow.md).

## Qualification Checklist

- A second identical sync call after completion starts a new run when
  `resultReplayMs` is 0, and replays within a positive window.
- A retry after `WORKFLOW_TIMEOUT` re-attaches to the same instance.
- A run past `deadline_ts` is cancelled before effects and reports
  `WORKFLOW_TIMEOUT`.
- A failed task returns `WORKFLOW_TASK_FAILED` with the stored message on the
  first call, not after the Gateway timeout.
- A nested call carries the parent link, inherits the deadline, and is
  refused above the delegation depth.
- A Gateway snapshot with a stale `bindingDigest` gets
  `WORKFLOW_DEFINITION_MISMATCH`.
- Publishing the same payload twice returns `unchanged`. Publishing a new
  digest for a published version is rejected.
- A failed binding publish leaves the previous Gateway snapshot active.
- The local stack works with `workflow-projection-sync` removed and no FDW
  server configured.
- `validate-contracts.mjs` passes.
- A binding published by someone other than the definition owner is
  `pendingApproval`, is left out of the Gateway snapshot, and becomes active
  only after the owner approves the reviewed digest.
- A name-only change keeps an approved binding active. A deadline change
  makes it pending again while the previous binding keeps serving.
- A revoked binding returns `WORKFLOW_POLICY_DENIED` on the next call.
- A user with `genai-admin` but not `workflow-admin` can call a
  workflow-backed Tool, and a user outside a binding's caller policy cannot.
- `workflow_invoke` makes no light-oauth call. The run's outbound and nested
  calls carry the original user token, and the stored token is gone after
  the run ends.
- A `workflow_start` run whose original token is still valid makes outbound
  calls with that token and does not call light-oauth. Once the token is
  within the margin, it switches to the exchanged token.
- No outbound call path uses `CredentialBroker::renew_for_run`.
- A binding with `invocationMode: async` or a human task is rejected by the
  normalizer.
- Concurrent and per-minute limits return `WORKFLOW_CAPACITY_EXHAUSTED` with
  `retryAfterMs`, and re-attaches do not count.
- Re-pinning to a new version with unchanged fields stays active; a version
  that exceeds `maximumParallelism` is rejected at publish; a version
  published with `reapprove` sends the binding to the owner.
- A sync write Tool with `derived` idempotency and `resultReplayMs` 0 is
  rejected by the normalizer.
- A client `tools/call` for `workflow_invoke` is refused by the Gateway.
- A nested workflow-backed Tool call is refused when the mTLS action dispatch
  is not wired, and admitted once it is.

## Future: Gateway to Workflow mTLS

Customers may run their own Workflow instance and connect it to the hosted
Gateway. That needs a zero-trust link: mutual TLS between the Gateway and
Workflow, on top of the user and app tokens. Parts already exist:

- `dual_identity` app profiles accept either pinned leaf fingerprints or a
  trusted CA.
- Workflow registers the Gateway's mTLS peer fingerprint in
  `workflow_gateway_owner_t` and checks it for nested calls.

Still to design:

- **Initial distribution.** How a new Workflow instance gets its first key and
  certificate and learns the Gateway's identity, without copying secrets by
  hand. One option: the instance generates its key locally and enrols with a
  one-time token through controller-rs or the Light Identity Issuer, which
  signs a short-lived certificate.
- **Rotation.** Short certificate lifetimes with automatic re-enrolment
  before expiry, and fingerprint updates in `workflow_gateway_owner_t`
  without an outage.
- **Revocation.** How a customer or the operator cuts off a compromised
  instance.

The `workflow_invoke` and publication contracts do not change when mTLS is
added.

## Decisions

1. Publication goes through the Gateway, which connects to Workflow. Per-host
   Workflow discovery through controller-rs comes later and does not change
   the contract.
2. Publication is not a workflow.
3. Nested calls use the existing parent action mechanism, with Workflow
   deriving child limits.
4. Admitted runs never re-read the binding row.
5. The derived idempotency key uses the user subject only.
6. The Gateway is the only caller of `workflow_invoke`. Agents use the
   workflow-backed Tool.
7. The workflow owner approves bindings from other Tool owners. The pending
   binding and the decision live in Workflow; the UI is in portal-view.
8. Bindings carry concurrency and rate limits that the owner approves.
9. Approval carries over to a new definition version when nothing it covers
   changed and the version fits the approved limits.
10. Write workflows can be Tools, under the idempotency rules in **Read-only
    and write Tools**.
11. Workflow-backed Tools are sync only. `workflow_invoke` passes the
    original user token and the Gateway app token, and nested calls forward
    the same user token. There is no light-oauth registration, token exchange
    or expiry check.
12. Every run uses the original user token until it is about to expire, then
    the LONG exchanged token if the run has one. The grant broker is removed.
13. The proposed `admissionLimits` defaults are accepted.
14. Tool permission is enough to call a workflow-backed Tool. The workflow
    owner can narrow it with the binding's caller policy.
15. Gateway-to-Workflow mTLS is future work (see **Future: Gateway to Workflow
    mTLS**).

## Open Questions

None at present.
