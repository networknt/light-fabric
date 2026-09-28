# Workflow Invoke And Tool Binding Publication

Status: Component implementation for issue #415; live qualification remains
unverified. Revised after the 2026-09-26 plan review (R1–R13).

This document defines two related changes to workflow-backed MCP Tools:

1. A new native `workflow_invoke` MCP operation on `light-workflow`. The
   Gateway calls it for every workflow-backed Tool. It admits the call against
   a Workflow-owned, owner-approved binding revision, waits for completion
   within the binding's deadline, and returns the output or a clear timeout or
   failure result.
2. Publication of workflow definitions, grants and Tool bindings to
   `light-workflow` through Workflow MCP operations, replacing the direct
   database sync.

The Gateway is the only caller of `workflow_invoke`. An Agent that needs a
workflow calls the workflow-backed MCP Tool on the Gateway like any other
Tool. A skill can guide the Agent to that Tool through `skill_tool_t`, but it
never calls the workflow directly.

It supersedes the **Workflow Start and Invocation API** section of
[Workflow-Backed MCP Tools](../light-gateway/workflow-backed-mcp-tools.md) for
workflow-backed Tools. `workflow_start` remains the entry point for editors,
schedulers and other asynchronous callers, as described in
[Start Workflow](start-workflow.md).

The Portal `StartWorkflow` command initializes and acknowledges both the saved
definition and its current grant set before it calls `workflow_start`. It
rechecks the acknowledged definition revision and digest after grant delivery
and passes that digest as `expectedDefinitionDigest`. Workflow still fences the
definition digest and enforces the live grants at admission and execution.
The request body's `idempotencyKey` takes precedence over the
`Idempotency-Key` header. If neither is supplied, each Start request generates
a fresh key and represents new work, even with identical workflow input. A
caller retrying one intended start must reuse an explicit key.

The Workflow Editor attempts synchronization when a user saves or publishes.
Portal keeps the desired definition and grant revisions alongside Workflow's
acknowledged revisions. If delivery fails after a local save, the editor shows
both revision pairs and the last error; **Sync now** retries with the current
user's bearer. If the status query also fails, the editor retains an
unconfirmed-sync warning and keeps **Sync now** available until a later status
read confirms both revisions. There is no periodic synchronization job. Start may also sync
pending revisions during the authenticated user request, and runs only after
the required revision is acknowledged.

## Operator setup

The local stack uses the main `all-in-lt/docker-compose.yml`. Provision the
Workflow run credential keyring through its idempotent runtime secrets init
service; `WORKFLOW_LONG_KEYRING_FILE` points to
`/run/secrets/run-credential-keyring.json`. Workflow refuses invoke admission
when a usable sealing key is unavailable. Configure
the Gateway application identity accepted by Workflow. All Portal publication,
definition and grant synchronization calls use the acting user's bearer in `Authorization`;
Gateway supplies its own application bearer in `X-Scope-Token` to Workflow.
At the Gateway entry, `Authorization` must contain a valid user bearer. If a
caller supplies `X-Scope-Token`, Gateway independently verifies it as an app
bearer; a missing scope header is allowed, and an invalid one is refused.
The scope token never substitutes for user authentication. The same user role
and MCP Tool access rules apply in either case. This path requires no mTLS.
JWT-only Gateway-to-Workflow operation is supported; mTLS is optional future
setup.

In Portal Rule Admin (`/app/rule/admin`), assign the established user role rule
to `workflow_definition_save` and `workflow_definition_grants_sync` in MCP
Gateway Setup (`/app/mcp/setup`) → Access Control. Allow `admin`, `host-admin`,
and `workflow-admin` for these two endpoints. An app bearer in `Authorization`
must be denied even if it carries a user-like claim.

On the other Workflow endpoint cards, add the established role-based rule and
assign roles: definition publish/retire to `admin`, `host-admin`,
`workflow-admin`, `genai-admin`; binding publish/retire to `admin`,
`host-admin`, `genai-admin`; binding get/list/decide/revoke to `admin`,
`host-admin`, `workflow-admin`, `genai-admin`. In Role Permission
(`/app/access/rolePermission`), assign the Portal commands
`decideWorkflowToolBinding`, `revokeWorkflowToolBinding`, and
`refreshWorkflowToolBindingStatus` to those four roles. Assign
`retireWorkflowToolBinding` to `admin`, `host-admin`, `genai-admin`.
Keep `retryWorkflowOperation` under its existing command ACL and original-user
check. Check each endpoint ID before saving.

Definition and grant synchronization travels from Portal through Gateway MCP
to Workflow's authenticated publication operations. The Portal needs no direct
connection to the `workflow-ops` database. The Gateway keeps
`workflow_invoke` internal: it is absent from client `tools/list` and direct
client `tools/call` is refused. Gateway still applies the published Tool ACL
before its internal call.

## Historical problem

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
  `PortalExecution` profile. The `WorkflowBacked` checks (`verify_binding`,
  orchestration and pinned dependency validation, approval evidence,
  deadline-aware admission) do not run.
- **Sync waiting lives in the Gateway.** The Gateway loops on
  `workflow_wait_result`, which returns null for failed or cancelled runs, so
  a terminal failure is indistinguishable from a slow run until the Gateway
  deadline. Lifecycle tool errors are then collapsed to
  `WORKFLOW_INVOCATION_UNAVAILABLE`.
- **Workflow data reaches Workflow by database access.** The development
  `workflow-projection-sync` Compose service runs
  `publish-workflow-projections.sh` every 30 seconds. It reads Portal tables
  over `postgres_fdw` and writes Workflow's `workflow_ops` schema directly:
  saved definitions, bindings, dependencies, endpoint targets and grants. In
  production the Workflow runtime may belong to another team, and the Portal
  cannot access its operations database. The sync is also asynchronous to
  Gateway publication, so the Gateway can serve a binding that Workflow has
  not seen yet, or the reverse.
- **Binding rows are mutable and read live.** An admitted run reads the
  active binding, its dependencies and its endpoint targets again at each task
  (`executor.rs` grant and endpoint reads, `bound_mcp.rs` dependency join).
  A republish or revocation during a run changes what the run may reach.

## Goals

- Workflow-backed Tools are admitted by Workflow from a Workflow-owned,
  immutable binding revision, not from Gateway-supplied runtime fields.
- An admitted run keeps the revision it was admitted with until it ends.
- One MCP call returns the output, a terminal failure, or a timeout that
  identifies the running instance.
- Portal writes definitions, grants and bindings only through Workflow MCP
  operations, and receives receipts it can pin in the Gateway configuration.
- The Gateway snapshot serves a Tool only when Workflow has an active revision
  with the same digests, so the two sides cannot skew silently.

## Non-Goals

- Changing `workflow_start` semantics for editor, scheduler or API callers,
  other than the saved-definition digest pin (see **Saved definitions**).
- Changing the workflow definition language or task execution.
- Agents calling `workflow_invoke` directly. They use the workflow-backed
  Tool, so there is one access-control and admission path.
- Nested workflow-backed Tool calls. They are refused until the mTLS action
  dispatch is restored (see **Parent action**).
- Running publication itself as a workflow. Publication records a pending
  revision and the owner decides separately (see **Workflow owner approval**).
- Backward compatibility with the projection sync. It is removed, not
  adapted.

## Tool Binding Model

A Tool binding connects one MCP Tool (`tool_t`) to one published workflow
definition version. The Portal creates it when a Tool is saved with
`executionPlacement=workflow` and a `workflowVersionRef`.
`WorkflowToolBindingNormalizer` fills defaults and rejects unsafe
combinations. The Portal stores the author's binding in its
`workflow_tool_binding_t`; Workflow stores each published version of it as an
immutable revision (see **Binding revisions**).

| Field | Meaning | Normalizer default |
|-------|---------|--------------------|
| `bindingId`, `toolId`, `toolName` | Portal binding identity and the stable Tool reference | from the Tool |
| `wfDefId`, `workflowVersion` | Pinned definition version | from `workflowVersionRef` |
| `definitionDigest`, `schemaDigest` | Hashes of the definition and its input schema | computed |
| `invocationMode` | Always `sync` for Tools (see **Sync only**) | `sync` |
| `syncWaitMs` | How long the caller waits for output | 20000, max 20000 for sync |
| `totalDeadlineMs` | Hard deadline for the run | 30000, max 30000 for sync |
| `executionClass` | Scheduling class | `interactive` for sync |
| `resultTextMode` | How output is rendered to MCP text | `compact-json` |
| `cancellationPolicy` (new) | What happens to a run past its deadline | `before-effects-only` |
| `idempotencyPolicy` | Key kind and replay windows | kind `derived`, `resultReplayMs` 0 for read-only Tools |
| `delegationPolicy` | Nested call rules | `maximumDelegationDepth` 1 |
| `runtimeBounds` | Attempts, nested calls, parallelism, bytes, cost units | 8, 8, 1, 1 MiB / 4 MiB / 1 MiB, 1000 |
| `admissionLimits` (new) | Concurrent runs and start rate, total and per user | see **Concurrency and rate limits** |
| `callerPolicy` (new) | Roles a user needs to reach the workflow through this Tool | empty: the Tool ACL decides |
| `toolAnnotations` (new) | The Tool's `readOnly` and `destructive` flags | from the Tool |
| `policyDigest`, `responsePolicyDigest` | Hashes of the admission and response profiles | computed |

Binding `cancellationPolicy` accepts exactly `before-effects-only`, `cooperative`,
and `disabled` in publication JSON and the Portal and Workflow binding tables.
The default is `before-effects-only`. Admission explicitly maps these strings
to `CancellationPolicy::BeforeEffectsOnly`, `Cooperative`, and `Disabled`.
The existing invocation enum serialization and invocation-row storage remain
`BEFORE_EFFECTS_ONLY`, `COOPERATIVE`, and `DISABLED`; the new binding contract
does not accept those uppercase spellings.

The database enforces `total_deadline_ms >= sync_wait_ms` and that sync mode
uses the interactive class.

Related records travel with the binding:

- **Dependencies** (`workflow_tool_dependency_t`): nested Tools that the
  workflow may call, with contract digest, authorization key and dispatch
  target.
- **Endpoint targets** (`workflow_endpoint_target_t`): the resolved HTTP
  endpoints that tasks may call, with allowed methods and authorization policy
  digest.
- **Task approval evidence** (`workflow_tool_approval_evidence_t`): one row
  per write task, generated by Workflow when the owner approves the revision
  (see **Task evidence**).

Grants (`workflow_tool_grant_t`) are not part of the binding. They belong to
the workflow definition: the Tool owner grants a workflow access to a Tool
(see **Grants**).

The binding has two consumers:

- The **Gateway** needs just enough to list and route the Tool: name, input
  schema, the pins and digests, the mode, and the wait time for its HTTP
  timeout. It gets these from `mcp-router.yml`.
- **Workflow** needs the full revision to admit a call. It gets it from
  publication (below) and treats it as authoritative.

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
the normalizer and Workflow admission (`rule_api.rs`, the sync nested-target
check) reject a sync binding unless every reachable Tool is `readOnly`, not
`destructive` and not `humanApprovalRequired`, so write workflows could only
be async Tools.

The read-only rule exists because a sync caller can give up before the run
finishes. An MCP client or Agent that times out usually retries. With a
read, a duplicate run costs capacity. With a write, the client cannot tell
whether the first attempt changed anything, and the retry may do it twice.

With `workflow_invoke` the retry re-attaches to the in-flight run or replays
its result, which removes that risk as long as the replay window covers the
client's retry horizon. The Tool's own annotations decide what the workflow
may do:

| Tool | Required |
|------|----------|
| Read-only (`readOnly: true`) | the workflow reaches only reads (matrix below) |
| Writes (`readOnly: false`) | `idempotencyPolicy.resultReplayMs` ≥ 600000 for every key kind |
| Destructive (`destructive: true`) | as for writes; clients see the flag and ask for confirmation |
| Contains a human task or reaches a `humanApprovalRequired` Tool | rejected; use `workflow_start` |

**Sync effect matrix.** Workflow checks each task of the pinned definition
against the outer Tool's annotations at binding publish and again at
admission:

| Task | Read-only Tool | Write Tool | Destructive Tool |
|------|----------------|------------|------------------|
| HTTP `GET`/`HEAD` | allowed | allowed | allowed |
| HTTP write method | rejected | allowed, with task evidence | allowed, with task evidence |
| Nested MCP Tool, `readOnly: true` | allowed | allowed | allowed |
| Nested MCP Tool, `readOnly: false` | rejected | allowed, with task evidence | allowed, with task evidence |
| Nested MCP Tool, `destructive: true` | rejected | rejected | allowed, with task evidence |
| Nested Tool with `humanApprovalRequired`, or a human task | rejected | rejected | rejected |

The static fit check at publish reuses the existing retry and budget envelope
calculation, so a write Tool's worst-case attempts still fit the deadline.

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

Workflow enforces the limits inside the admission transaction, under a
per-Tool advisory lock so concurrent admissions cannot both pass. It counts
non-terminal runs and starts in a sliding one-minute window for the Tool
across all of its revisions, so a republish does not reset the counters. A
re-attach or replay does not count as a new start. When a limit is hit,
Workflow returns `WORKFLOW_CAPACITY_EXHAUSTED` with `retryAfterMs`. The
Gateway permit pool still protects the Gateway itself, but Workflow's limits
are the ones the owner approves.

## Publication Through Workflow MCP

### Operations

Workflow exposes these operations on its native MCP endpoint, next to
`workflow_start`. They are declared in
`contracts/workflow-admin/workflow-tools-list-full.json` with schemas and examples like
the other workflow-admin tools.

| Tool | Purpose |
|------|---------|
| `workflow_definition_save` | Upsert the saved (editable) head of a definition |
| `workflow_definition_publish` | Publish one immutable definition version |
| `workflow_definition_retire` | Stop new admissions to a definition version |
| `workflow_definition_grants_sync` | Replace the Tool grants of one definition |
| `workflow_binding_publish` | Publish a binding revision with its dependencies and endpoint targets |
| `workflow_binding_retire` | Retire a Tool's binding, called by the Tool owner |
| `workflow_binding_get` | Read one revision with its status and decision history |
| `workflow_binding_list` | List revisions for definitions the caller owns, filterable by status |
| `workflow_binding_decide` | Approve or reject a pending revision, called by the definition owner |
| `workflow_binding_revoke` | Withdraw approval of an active revision, called by the definition owner |

Every call carries the acting user's token in `authorization`. The Gateway adds its own token in
`x-scope-token`, and Workflow checks it against
`workflow.invocation.allowedCallerServiceIds`, as for `workflow_start`.

The Portal reaches these tools through the same Gateway `/mcp` route that
`StartWorkflow` uses, and the Gateway connects to Workflow. The Portal never
connects to Workflow or to the Workflow database directly. Today the Gateway
uses one configured Workflow endpoint. When each host runs its own
`light-workflow` instance, the Gateway will locate it through controller-rs
service discovery; the publication contract does not change.

### Request host and identity

The ten definition-publication and binding-management tools carry a required
`hostId` as a target-host assertion. It does not supply identity or grant
cross-host access. Workflow authenticates first and rejects a mismatch with
`WORKFLOW_POLICY_DENIED` before any scoped read, mutation or operation-receipt
lookup/replay. Database scope comes from the verified user host. The request
body, user bearer, and Gateway scope bearer must identify the same host.
Gateway caller-service validation remains required for every call.

`workflow_invoke` has no `hostId` argument; its host is derived from trusted
invocation context. The manifest retains `identitySource: trustedInvocationContext`.
The input-schema validator permits only the exact root `hostId` paths on the
ten tools, without relaxing its identity/fencing guard for existing tools,
alternate spellings or nested fields.

The `owner` objects on definition save/publish describe user-authenticated
resource metadata, not caller identity. Only save changes current ownership;
authorization uses the verified caller and stored owner. The `role` field on
binding list is only the `owner`/`requester` relationship filter relative to
that caller. These are exact tool/path validator exceptions with constrained
schemas. A sync payload's `actor` is audit metadata and grants no authority.

### Gateway assertion

The user token identifies who clicked. Gateway checks that user's endpoint
permissions before forwarding, and Workflow independently checks the user host
and Gateway service identity. A payload `actor` cannot replace either token.

All definition, grant, and binding operations carry the acting user's bearer
in `Authorization`. Gateway verifies that user and the endpoint ACL. On the
Gateway-to-Workflow request, Gateway supplies its own app bearer in
`X-Scope-Token`; Workflow verifies its service identity, host, and environment
independently from the user bearer and checks that the user host matches the
request host. `hybrid-command` does not mint an application token for Workflow.
The payload `actor` remains audit metadata, never a credential.

- A missing or invalid user or Gateway token returns `isError` with
  `WORKFLOW_POLICY_DENIED`.

`workflow_binding_get`, `workflow_binding_list`, `workflow_binding_decide`
and `workflow_binding_revoke` use the same user and Gateway tokens. Workflow checks the
caller against the definition owner it stored, so they do not depend on the
Portal's word.

**Ownership.** The definition owner (`owner_user_id`, `owner_position_id`) is
set by `workflow_definition_save`, so an ownership change in the Portal
reaches Workflow through the publisher-authenticated save. Active approvals
stay active after a transfer. Pending revisions move to the new owner.
Carry-over requires that the owner recorded on the basis revision equals the
current owner (see **Carry-over**). A position holder is checked from the
position claims of the user token, so position changes take effect when the
user's token is renewed.

### Saved definitions

`workflow_start` runs the saved, editable head of a definition
(`wf_definition_t`), and the FDW sync kept that head in step with the Portal.
Without the sync, Workflow needs its own write path for it.

`workflow_definition_save` input:

```json
{
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "wfDefId": "0198a3c2-...",
  "sourceRevision": 7,
  "actor": "steve",
  "namespace": "claims",
  "name": "summarize-claim",
  "version": "1.2.0",
  "definition": "document:\n  dsl: 1.0.0\n  ...",
  "lifecycleStatus": "PUBLISHED",
  "catalogVisible": false,
  "owner": { "userId": "...", "positionId": "..." },
  "active": true
}
```

`definition` is the YAML text as stored in the Portal. `catalogVisible` is a
required boolean independent of lifecycle status; a published definition may
remain private. Portal sends its stored value from the same consistent snapshot
as the other save fields and normalizes legacy database NULL to false. Visibility
is outside the definition-text digest. The saved head accepts at most 126
characters for `actor`, `namespace` and `name`, and 20 for `version`, matching
the existing table. Published immutable versions retain a 64 character version
limit. `sourceRevision` is
the Portal definition's aggregate version. Workflow keeps the last applied
revision on the head row:

- a lower revision returns `result: "stale"` and changes nothing, so a
  delayed older save can never restore older text, owner or deletion state;
- the same revision with the same content and visibility returns `unchanged`;
  with different content or visibility, `WORKFLOW_IDEMPOTENCY_CONFLICT`;
- a higher revision upserts the head and returns `saved`.

Every receipt, `stale` included, returns `appliedRevision` (the revision
Workflow now holds) and the `definitionDigest` of the stored head at that
revision, so the pair always describes Workflow's state. `active: false`
records a delete. The Portal delivers saves through the durable
synchronization described below.

**Save-then-start.** A start must run exactly what the user saved:

1. Portal `StartWorkflow` checks that Workflow has acknowledged the current
   Portal revision of the definition. If not, it delivers the save at once
   and fails the start if that delivery fails.
2. It also initializes and acknowledges the current full grant set, including
   legacy grants with no sync row. If grant delivery fails, it does not start.
   After grant delivery it rechecks the acknowledged definition revision/digest
   pair. This is a readiness check; grants remain live at runtime.
3. It then calls `workflow_start` with the new optional
   `expectedDefinitionDigest`, set to the digest acknowledged together with
   that revision. Workflow
   compares it to the head it loads and returns
   `WORKFLOW_DEFINITION_MISMATCH` on a difference.

The `workflow_start` MCP catalog input schema must declare this optional
digest with `^sha256:[0-9a-f]{64}$`. Gateway validates arguments against its
published tool schema before calling Workflow. The catalog must first be
reimported into Portal's MCP API endpoint (`api_endpoint_t.tool_schema`), then
the Gateway Tool publication must be previewed and published and its config
snapshot activated. Rebuilding and restarting Workflow or republishing Gateway
Tools alone leaves an older Portal endpoint schema in place.

Every Portal start path goes through `StartWorkflow`, including the workflow
editor's Start button. A body idempotency key takes precedence over the header.
With neither, Start means new work and allocates a fresh key; callers retrying
one intended start must reuse an explicit key.

`workflow_definition_publish` never changes the head.

**Digest parity.** The Rust digest is `sha256:` plus
`execution_runner_protocol::canonical_sha256` of the YAML parsed to JSON. The
Java `WorkflowDefinitionDigest` uses SnakeYAML with YAML 1.1 rules. The two
disagree on some inputs: `yes`/`no`/`on`/`off`, unquoted timestamps, `1` vs
`1.0`, merge keys and explicit nulls. Workflow's digest is authoritative, and
Portal compares its own only as a precheck. A shared fixture set covers these
cases, and both sides must agree on it before the Portal precheck is trusted.

### Definition publish

Input:

```json
{
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "wfDefId": "0198a3c2-...",
  "namespace": "claims",
  "name": "summarize-claim",
  "version": "1.2.0",
  "definition": "document:\n  dsl: 1.0.0\n  ...",
  "expectedDefinitionDigest": "sha256:...",
  "bindingApproval": "carryOver",
  "owner": { "userId": "...", "positionId": "..." },
  "operationId": "0198a3c3-..."
}
```

`definition` is YAML text, as in save.

Workflow must:

1. Parse the definition and recompute its definition and input schema
   digests. It rejects the call if the result differs from
   `expectedDefinitionDigest`.
2. Run the generic definition validation (runtime definition and CEL
   checks). Binding-specific limits such as effect mode and budget are
   checked at binding publish, not here.
3. Store the version in a version table keyed by
   `(hostId, wfDefId, version)`. Versions are immutable.

Receipt:

```json
{
  "result": "published",
  "status": "active",
  "wfDefId": "0198a3c2-...",
  "version": "1.2.0",
  "definitionDigest": "sha256:...",
  "schemaDigest": "sha256:...",
  "bindingApproval": "carryOver"
}
```

**Version states.** A version is `active` or `retired`; `retired` is
terminal.

- Republishing the same digest returns `result: "unchanged"` with the current
  status, including `retired`.
- A different digest for an existing version is rejected with
  `WORKFLOW_DEFINITION_MISMATCH`.
- `workflow_definition_retire` is refused while an active binding revision
  pins the version. Pending revisions on it become `withdrawn`, each with a
  decision record.
- Admission against a retired version returns
  `WORKFLOW_DEFINITION_RETIRED`. Running runs finish.

Existing binding rows are not backfilled with version rows. They are
admissible again only after the Portal republishes them.

### Grants

A grant lets a workflow call a Tool through LightAPI. The Tool owner approves
it through `RequestWorkflowToolAccess` and `DecideWorkflowToolAccess`, so it
belongs to the definition, not to a binding.

`workflow_definition_grants_sync` input:

```json
{
  "hostId": "...",
  "wfDefId": "...",
  "sourceRevision": 12,
  "actor": "steve",
  "grants": [
    {
      "grantId": "...",
      "toolId": "...",
      "toolVersion": "1.0.0",
      "lightapiDigest": "sha256:...",
      "allowedEnvironments": ["dev"]
    }
  ],
}
```

It replaces the full grant set of that definition in one transaction.
`sourceRevision` works as in save: a lower revision is `stale` and changes
nothing, so a delayed older set can never restore a removed grant. The
Portal delivers it after `DecideWorkflowToolAccess`, after an access
revocation, and in **Publish Selected** before binding publish.

Grants are read live, not pinned to a run. A revocation is enforced once
Workflow acknowledges the grant set that removes it; until then the Portal
shows "Sync pending" on the access list. After that, every later dispatch
that needs the grant is refused, including the next call of a run already in
flight. An operation already dispatched is not undone.

### Synchronization

Definition saves and grant sets are delivered by authenticated user actions:

- The Portal keeps a sync row per definition and kind (`definition`,
  `grants`) with a desired revision and the user of the last change, written
  in the same transaction as the event that changed the data, and the
  revision and digest Workflow last acknowledged.
- Delivery reads the revision, the user and the data in one consistent
  database snapshot, so content is never paired with another revision's
  number. When several grant changes are delivered as one set, the user of
  the last change is sent as `actor`.
- Save and Publish in the Workflow Editor attempt delivery with the current
  user bearer. A committed local revision remains pending if delivery fails.
  `getWfDefinitionById` returns desired and acknowledged definition and grant
  revisions, and the editor shows the difference with a **Sync now** action.
  `syncWfDefinition` retries both kinds under the current user bearer. There
  is no scheduled synchronization worker or stored user credential.
- The Portal stores the acknowledged revision and digest together and only
  moves them forward, so a late receipt for an older revision is ignored.
  Errors are recorded and shown in the UI.
- Definitions and grants that existed before this change get their sync row
  on first use: the definition is delivered before its first start, and the
  full grant set before its first binding publication.
- A fresh or rebuilt sync row may start at or below the revision Workflow
  already holds. Workflow reports its stored revision in a `stale` receipt or
  in the equal-revision conflict. For grants, which the Portal owns, the
  Portal moves its revision above Workflow's and sends its current set again.
  For a definition, whose revision is the Portal aggregate version, the sync
  stops with an error for investigation until a later edit moves the
  revision past Workflow's.

### Lost receipts

**Final review amendment (2026-09-27):** remediation is specified in
`implementation/light-workflow/workflow-invoke-execution/final-remediation-handoff.md`.
The ledger classifies operation outcome separately from retryability. Proven
rejection of the exact stored operation may close it as failed. Authentication,
transport, Gateway, unreadable-response and unproven JSON-RPC errors preserve
pending state. Initially, definitive rejection is limited to validated binding
publish/retire VERSION_CONFLICT responses from paths that check the original
receipt before rejecting its stale expected version. Retry never edits the
stored request. Other errors do not prove the original outcome.

**F2 request-validation evidence (2026-09-27):** For binding publish and
retire, a negative `expectedAggregateVersion` is a request-only validation
failure. The authenticated publisher and host checks precede parsing. A
parseable request with `hostId` and `operationId` reaches the serialized
`operation_begin` insert/row lock and exact tool/request-digest comparison
before this check. An existing success receipt wins. The rejection is stored
under that same operation ID while holding the operation row lock, so an
overlapping attempt and later replay return the same rejection even if
validation rules change. The error remains `WORKFLOW_INPUT_INVALID`,
`afterEffect=false`, and includes additive `details.requestValidation`:
`{version:1, discriminator:"negativeExpectedAggregateVersion",
operationId:"<UUID>", toolName:"workflow_binding_publish|workflow_binding_retire"}`.
Portal accepts this evidence only from a Workflow Tool error for the matching
stored operation/tool and a numeric negative expected version in the stored
request. Missing, malformed, mismatched, or unmarked errors remain unknown.

#### Additive binding request-validation evidence (version 2)

After authenticated host verification and serialized operation lookup, binding
publish may store a request-only rejection for either `bindingFields` or
`bindingReach`. Evidence is
`{version:2, discriminator, operationId, toolName:"workflow_binding_publish", requestSection}`.
`bindingFields` requires `requestSection:"binding"`; `bindingReach` requires
`requestSection:"reach"` and covers the typed `dependencies` and
`endpointTargets` arrays together. The section identifies the immutable request
member inspected by Workflow. The rejection stores the
original `WORKFLOW_INPUT_INVALID` code, exact message, and complete evidence;
replay returns those stored values. Portal accepts it only for a Workflow Tool
error with `afterEffect=false`, a matching operation and tool, a nonnegative
expected version, and the named section present with its contract type in its
stored exact request. Portal does not reinterpret a bare business error code as
proof. Both versions require exactly their documented evidence members; extra
members are malformed and remain unknown. The earlier version 1 negative-version
marker remains supported.

`bindingFields` covers every explicit field, bound, policy, digest-format,
role, annotation, and schema-type rejection in `validate_binding`.
`bindingReach` covers reach size, dependency field/digest/policy/uniqueness,
and endpoint field/digest/uniqueness/URI/method/document rejections in
`normalize_payload`. Digest *format* checks use immutable supplied strings.
`digests` serialization and canonical-hash computation errors do not establish
invalid input and carry no evidence. Definition, grants, owner, current head,
static fit, runtime configuration, database, authentication, transport, and
other state-dependent failures remain unknown unless separately proven.
Deserialization errors cannot safely identify and fence an operation, so they
remain unknown. Other `WORKFLOW_INPUT_INVALID` paths remain unknown, including
state-dependent missing binding and limits. The existing validated
`VERSION_CONFLICT` rule remains independent.

A validated definition publication receipt with `result=unchanged` and
`status=retired` is a completed D21 outcome. The definition prerequisite for
binding publication additionally requires `status=active`; a retired receipt
is neither cached as active nor followed by a binding send. Original-user
Retry replays the retired receipt, and publishing another version is deliberate
new work. Gateway-removal retirement results retain Portal-owned identity,
status, code and message; unconfirmed results expose only validated recovery
metadata. A forbidden Retry means only that the caller is not the original
requester. It does not establish completion, failure or expiry.

**UI recovery follow-up:** A recovered definition receipt with either
`result=published` or `result=unchanged` and `status=active` enables an explicit
binding-publication continuation; it never sends the binding automatically.
When the authenticated Portal ledger returns a stored failed operation, Portal
adds `metadata.operationState="failed"` to the command error while preserving
the stored business error code. This Portal-owned marker, rather than the code
or retryable flag, lets the UI stop Retry and offer deliberate new work.
Unproven remote errors do not receive the marker.
Ledger database errors and unknown operation-prefixed Retry errors remain
recoverable with the original operation ID; neither establishes failure or
expiry. The UI handles explicit expiry, forbidden Retry, and not-found
responses separately.

Concurrent finalization rereads the committed outcome when its guarded update
loses. Remote reconciliation happens outside the short receipt/event/projection
transaction. Publication event-version allocation is serialized using the
existing global aggregate identity. Wrong-user Retry is forbidden regardless
of state and must not expose receipts or masquerade as pending.

Every other mutating call the Portal makes to Workflow (definition publish
and retire, binding publish, retire, decide and revoke) goes through a
Portal operation ledger. The Portal stores the `operationId`, the exact
request and the requesting user before calling. After a timeout the
operation shows "Unconfirmed — Retry". Only the same user can retry. Retry
is its own Portal command that names the original `operationId`; it carries
that user's current token and resends the same request with the same
`operationId`, and it never creates a new operation. Workflow returns the
stored receipt, so the retry resolves the original operation instead of
making a second mutation. Running the original command again while the
operation is unconfirmed sends nothing and points to Retry. Another user cannot take over that
operation; they see who has it unconfirmed. This covers one operation
stream (one operation on one subject). Other authorized operations on the
same Tool or definition are not blocked; Workflow's own concurrency checks
govern them.

An unconfirmed operation expires 29 days after it was created. Workflow
drops its record 30 days after its own creation. The deadline never moves.
Every retry checks it first, and an expired operation is never resent; it
shows "Expired; remote outcome unconfirmed", because Workflow may or may not
have applied it. The UI offers Refresh status and then a separately
labelled action to publish (or decide, revoke, retire) again. That action is
new work and creates a new operation.

The one-day margin is an operational condition, not a guarantee: a resend
is safe when the clock skew between Portal and Workflow plus the time from
the Portal's expiry check to Workflow processing the request is under 24
hours. Workflow does not enforce the Portal deadline itself.

These operations need a user token, and the ledger does not store tokens, so
nothing retries them in the background. A receipt updates the Portal
projection only when its aggregate version is newer than what the Portal
already holds.

### Binding revisions

Every Workflow `workflow_tool_binding_t` row is an immutable revision.
`binding_id` is the revision id, generated by Workflow. The Portal's binding
id is stored as `source_binding_id`.

A revision has a `revision_status`:

| Status | Meaning |
|--------|---------|
| `pendingApproval` | Waiting for the definition owner |
| `approved` | The one active revision for the Tool (`active = true`) |
| `rejected` | Refused by the owner, with a comment |
| `superseded` | Replaced by a newer approved or pending revision |
| `withdrawn` | Pending on a definition version that was retired |
| `retired` | Retired by the Tool owner |
| `revoked` | Approval withdrawn by the owner |
| `legacy` | Written by the projection sync before this change; never admitted |

`active` is true only for `approved`, and at most one revision per Tool is
active and at most one is pending. Dependencies, endpoint targets and task
evidence are written once per revision and never updated.

An admitted run records its `binding_id`. Every later read in the run uses
that revision without checking `active`: the grant join and endpoint lookup
in `executor.rs`, the dependency join in `bound_mcp.rs`, and
`validate_pinned_dependencies`. Revoking or superseding a revision therefore
stops new admissions but not runs in flight; they finish. The idempotency
replay trigger also uses the pinned revision's policy.

Each Tool has a publication head row that holds its aggregate version, the
active revision id and the pending revision id. Every publication operation
locks it (see **Concurrency**).

### Binding publish

The input carries the normalized binding and the records Workflow needs. The
Portal resolves endpoint targets from `tool_t`, `api_*` and the permission
tables before the call, which the projection script used to do over the FDW.
Workflow receives only resolved values.

```json
{
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "binding": {
    "sourceBindingId": "...",
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
    "admissionLimits": { "maximumConcurrentRuns": 20, "...": "..." },
    "callerPolicy": {},
    "toolAnnotations": { "readOnly": true, "destructive": false },
    "policyDigest": "sha256:...",
    "responsePolicyDigest": "sha256:..."
  },
  "dependencies": [],
  "endpointTargets": [],
  "expectedAggregateVersion": 3,
  "operationId": "0198a3c4-..."
}
```

There is no `approvalEvidence` in the input. Workflow generates task evidence
itself when the revision is approved.

Workflow must:

1. Require that the pinned definition version is published, active, and that
   its digests match.
2. Validate the binding fields and constraints, including the sync effect
   matrix for the Tool's annotations.
3. Run `validate_orchestration_definition` and `validate_pinned_dependencies`
   against the payload, and the static fit check (see **Carry-over**).
4. Check each endpoint target against Workflow's own destination policy. The
   Workflow owner can refuse a target that the Portal has resolved.
5. Compute `bindingDigest` and `approvalDigest` (see **Digests**).
6. Decide the status (see **Status rules**), insert the revision and its
   related rows, update the Tool head, and store the receipt, all in one
   transaction.

Receipt:

```json
{
  "result": "published",
  "status": "active",
  "toolId": "...",
  "bindingId": "<revision id>",
  "sourceBindingId": "...",
  "workflowVersion": "1.2.0",
  "definitionDigest": "sha256:...",
  "bindingDigest": "sha256:...",
  "approvalDigest": "sha256:...",
  "aggregateVersion": 4,
  "decisionId": "..."
}
```

`status` is `active` or `pendingApproval`. `decisionId` is present when the
publish itself recorded a decision (self-approval or carry-over).

### Digests

**`bindingDigest`** identifies a revision. It is the canonical SHA-256 of:

- every binding field except `sourceBindingId`, `operationId` and
  `expectedAggregateVersion`;
- the dependencies, sorted by `(authorizationToolName, nestedToolId,
  nestedToolVersion)`;
- the endpoint targets, sorted by `endpointRef`, with methods uppercased,
  deduplicated and sorted, and environments sorted.

Null or absent fields are omitted. A duplicate set key is rejected, not
merged.

**`approvalDigest`** covers the fields that change load or behaviour: mode,
`syncWaitMs`, `totalDeadlineMs`, execution class, cancellation, idempotency
and replay, delegation, runtime bounds, admission limits, caller policy and
`toolAnnotations`. It excludes reach (dependencies and endpoint targets),
which carry-over compares as sets, and the definition version. Changing the
Tool name, description or input examples changes neither what the owner
approved nor the approval digest.

The previous `payloadDigest` is removed.

### Publish ordering

The Portal **Publish Selected** action works per Tool:

1. `workflow_definition_save` and `workflow_definition_publish` for each
   pinned definition version, and `workflow_definition_grants_sync` for each
   definition involved.
2. `workflow_binding_publish` for each workflow-backed Tool in the selection.
3. Build `mcp-router.yml`:
   - a Tool whose receipt is `active` is included with the receipt's
     `bindingDigest` and `definitionDigest`;
   - a Tool whose publish is pending or failed keeps its previous Gateway
     entry, if it has one, or stays out;
   - the rest of the selection is promoted normally.

The result dialog lists every Tool with its outcome, so one pending or failed
Tool does not block the others, and the Gateway never routes to a revision
that Workflow has not made active.

Retirement runs in the reverse order. The Portal removes the Tool from the
Gateway snapshot first, then calls `workflow_binding_retire`. Runs in flight
keep their pinned revision and finish. New `workflow_invoke` calls for the
retired Tool are rejected.

If Gateway removal was staged but retirement remains unconfirmed until its
Portal operation expires, the requester may start explicit new work with the
Portal `RetireWorkflowToolBinding {hostId, instanceId, toolId,
expectedAggregateVersion}` command. Portal checks that the Tool is absent
from that instance's current staged Gateway publication and refuses retirement
if it was re-added. The caller supplies the Workflow Tool-head version read
after Refresh status; Portal rejects a changed head and never substitutes a
newer version. A live pending retirement returns
`WORKFLOW_OPERATION_PENDING`. Once the old operation expires, this command
allocates a new operation ID and uses the existing D21 ledger, publisher-token
path, and retirement completion event. `RetryWorkflowOperation` still targets
only the old operation and sends nothing after expiry. Gateway removal is not
staged again by this command. Snapshot activation and live routing remain
separate operator steps.

**Preview.** The Portal stores `published_request_digest` on its binding row:
the canonical digest of the binding publish input it sent, without
`operationId` and `expectedAggregateVersion`. Preview and the server-side
`PublishGatewayTools` include a Tool only when the payload rebuilt from the
current Portal rows has that digest and the stored status is active. An edit
since the last publish therefore shows as "needs publish", not as served.

### Concurrency

Each Tool has a head row in `workflow_tool_publication_t` with
`aggregate_version`, `active_binding_id` and `pending_binding_id`. Every
publication operation:

1. inserts the head with `ON CONFLICT DO NOTHING`, then selects it
   `FOR UPDATE`, which also serializes the first publication of a Tool;
2. locks in a fixed order: the definition version row first (`FOR SHARE` for
   publish and decide, `FOR UPDATE` for retire), then the Tool head.

`expectedAggregateVersion` applies to binding publish and binding retire. A
mismatch is a conflict, with the same semantics as other aggregate writes.
Decide and revoke pin `expectedBindingDigest` instead, so the owner acts on
exactly the revision they reviewed.

**Operation idempotency.** Every write carries an `operationId`. Workflow
stores `(hostId, operationId)` with the tool name, a request digest and the
receipt. A repeat with the same request returns the stored receipt. A repeat
with a different request returns `WORKFLOW_IDEMPOTENCY_CONFLICT`. Records are
swept after 30 days.

The binding publish receipt includes optional `carryOverDeniedReason` when
carry-over is denied and the new revision is `pendingApproval`. Active publish
receipts, including self-approval and successful carry-over, omit the field.
The operation record stores the complete response, so replay returns the
original reason unchanged.

Migration 0019 adds immutable `input_schema` and `output_schema` JSON columns
to each binding revision, preserving the optional Tool schemas covered by
`bindingDigest`. It also stores `carry_over_denied_reason` on the revision.
A new operation returning an unchanged pending revision includes that stored
reason, and `workflow_binding_get` reads it after operation receipts expire.
Binding get and list use read-only `REPEATABLE READ` transactions so their
head pointers, revision statuses, decisions, items and counts come from one
committed snapshot.

After a timeout or a lost receipt, the Portal refreshes from
`workflow_binding_get` and reconciles status, digests and aggregate version.

### Workflow owner approval

Binding a Tool to a workflow lets a new audience start that workflow, with a
deadline and budget the Tool author chooses. The workflow owner is
accountable for that load and for what the workflow does, so the owner must
approve a binding created by someone else. This is the reverse of the
existing Tool access approval:

| Direction | Requester | Approver | Mechanism |
|-----------|-----------|----------|-----------|
| A workflow calls a Tool | Workflow author | Tool owner | `RequestWorkflowToolAccess` → approval workflow → `DecideWorkflowToolAccess` → `workflow_definition_grants_sync` |
| A Tool invokes a workflow | Tool author | Workflow owner | `workflow_binding_publish` → pending revision in Workflow → `workflow_binding_decide` |

The pending revision and the decision are held by Workflow, not the Portal.
Workflow belongs to the owner's team, and the owner's decision reaches it
with the owner's own user token, so Workflow does not have to trust the
Portal's word that approval happened.

#### Status rules

`workflow_binding_publish`:

- **unchanged** when the new `bindingDigest` equals the active or pending
  revision's digest; the existing receipt is returned.
- **active (selfApprove)** when the publishing user is the definition owner.
  The previous active and pending revisions become `superseded`.
- **active (carryOver)** when the carry-over rules below hold.
- **pendingApproval** otherwise. A previous pending revision becomes
  `superseded`. The active revision, if any, keeps serving.

`workflow_binding_decide` with `approve` revalidates the revision inside the
transaction (definition version active, static fit, effect matrix) and then
activates it; the previous active revision becomes `superseded`. `reject`
requires a comment.

`workflow_binding_revoke` sets the active revision to `revoked`. New
`workflow_invoke` calls for the Tool return `WORKFLOW_POLICY_DENIED` with
"binding revoked by workflow owner" until the Portal removes the Tool from
the Gateway. Runs in flight finish.

`workflow_binding_retire` sets the active revision to `retired` and a pending
one to `withdrawn`.

#### Carry-over

The definition version is not part of the approval digest. When the Tool
author re-pins to a newer version of the same definition, the approval of
the current active revision carries over if all of these hold:

- same host, Tool and `wfDefId`;
- the owner recorded on the active revision equals the current definition
  owner;
- equal `approvalDigest`;
- the new reach is a subset of the active revision's reach, where a
  dependency is keyed by `(nestedToolId, nestedToolVersion, contractDigest,
  authorizationToolName)` and a target by `(endpointRef, endpointUri, sorted
  allowedMethods)`;
- the new version's write tasks are a subset of the active revision's, keyed
  by qualified task name, kind, and method or Tool;
- the new version passes the static fit check; and
- the new version was published with `bindingApproval: carryOver`.

Otherwise the revision is `pendingApproval`.

A heavier new version cannot overload the workflow under a carried-over
approval, because the approved limits still apply to it. To stop that from
surfacing as runtime failures, `workflow_binding_publish` checks the
version's static requirements against the binding: fork width against
`maximumParallelism`, declared nested calls against `maximumNestedCalls`, and
the retry and budget envelope against `maximumTaskAttempts` and the deadline.
A version that cannot fit is rejected with a message that names the limit.
The author raises that limit, which changes the approval digest and sends the
revision to the owner.

The owner knows best when a version is heavier, so
`workflow_definition_publish` takes `bindingApproval` of `carryOver`
(default) or `reapprove`. With `reapprove`, every revision that pins that
version goes to the owner.

`workflow_binding_decide` input:

```json
{
  "hostId": "...",
  "bindingId": "<revision id>",
  "expectedBindingDigest": "sha256:...",
  "decision": "approve",
  "comment": "Approved for claims read-only lookups",
  "operationId": "..."
}
```

#### Decision history

Every status change appends a row to `workflow_tool_binding_decision_t`:
decision id, Tool, revision, action (`approve`, `reject`, `revoke`, `retire`,
`supersede`, `withdraw`, `carryOver`, `selfApprove`), actor, comment,
approval digest, time and operation id. The table is append-only: the runtime
role has `INSERT` and `SELECT` only. `workflow_binding_get` returns it as the
revision history.

#### Task evidence

`validate_approval_evidence` requires, for each write task (a non-`GET`/`HEAD`
HTTP call, or an MCP call to a Tool that is not `readOnly`), the task's
`approvalEvidenceDigest` metadata and an active evidence row for
`(host, binding_id, task_name, digest)`. The projection sync never copied
these rows, so write tasks could not be admitted.

When a revision becomes active by self-approval, approval or carry-over,
Workflow writes one evidence row per write task of the pinned version, with
`evidence_digest` set to the task's `approvalEvidenceDigest` and
`approved_by` set to the definition owner. A write task without
`approvalEvidenceDigest` metadata fails binding publish. The evidence table
keeps its current shape.

#### UI flow

All screens are in portal-view. Portal commands call the Workflow tools
through the Gateway with the acting user's token, and store the returned
status on the Portal `workflow_tool_binding_t` row so lists do not need a
Workflow call. The review form reads the live revision from Workflow.

**Tool author, GenAPI Admin → Tool**

1. The author creates or edits a Tool with `executionPlacement=workflow` and
   picks a published workflow version. The form shows the definition owner.
   When the author is not the owner it shows "This binding needs approval
   from &lt;owner&gt; before it can be served."
2. The author runs **Publish Selected**. The result dialog lists each Tool:
   "Published", "Waiting for approval from &lt;owner&gt;", or the Workflow
   error. The rest of the batch is published normally.
3. The Tool list shows a binding status column: Active, Pending approval,
   Rejected, Revoked, Needs publish. Rejected and Revoked show the owner's
   comment. The author can edit and publish again, which creates a new
   pending revision.
4. After approval the status becomes "Approved, not yet on Gateway" until
   the next **Publish Selected** includes it. Gateway publication stays an
   explicit action, as it is today.

**Workflow owner, Workflow → Definitions**

1. The definition list shows a "Tool bindings" column with a badge for the
   pending count, for example "2 pending". The owner can filter the list to
   definitions with pending revisions.
2. The row action "Tool Bindings" opens a list of revisions for all versions
   of that definition: Tool name, Tool owner, definition version, status,
   requested or decided time and who decided.
3. Selecting a pending revision opens the review form, loaded with
   `workflow_binding_get`:
   - **Requester:** Tool name, description, Tool owner, host.
   - **Target:** definition name, version and digest.
   - **Execution:** mode, sync wait, total deadline, execution class,
     cancellation policy, read-only and destructive flags.
   - **Limits:** idempotency and replay window, delegation depth, task
     attempts, nested calls, parallelism, request, intermediate and result
     bytes, cost units, admission limits, caller policy.
   - **Reach:** dependencies, endpoint targets and write tasks.
   - **Change:** when an active revision exists for the Tool, the changed
     fields are highlighted against it.
   - A comment box and **Approve** / **Reject** buttons. Reject requires a
     comment.
4. An active revision has a **Revoke** action with a required reason.

**Worklist.** A pending revision also appears in the owner's Worklist, like
Tool access requests do today. Opening it goes to the same review form.

### What is removed

- The `workflow-projection-sync` service in the Compose files.
- `crates/workflow-store/deployment/publish-workflow-projections.sh`, and the
  `postgres_fdw` server and user mapping it needs.
- Any documentation step that tells operators to run the sync.

### Workflow schema changes

The writer changes from the FDW script to the publication handlers, and the
tables change with it (migration `0018`):

- `workflow_tool_binding_t` gains `source_binding_id`, `revision_status`,
  `binding_digest`, `approval_digest`, the owner snapshot
  (`owner_user_id`, `owner_position_id`), `requested_by`, `requested_ts`,
  `approved_by`, `approved_ts`, `approval_basis_id`, `cancellation_policy`,
  `admission_limits`, `caller_policy` and `tool_annotations`. A check
  enforces `NOT active OR revision_status = 'approved'`. Partial unique
  indexes allow one active and one pending revision per Tool.
- The `UNIQUE (host_id, tool_id, workflow_version)` constraint on
  `workflow_tool_binding_t` is dropped, so a Tool can have several revisions
  on the same definition version (a legacy row plus its republish, or a
  changed binding). Foreign keys use `(host_id, binding_id)`.
- `workflow_endpoint_target_t` is already keyed by `(host_id, binding_id,
  endpoint_ref)` since migration 0005, so each revision has its own targets.
  Migration 0018 preserves this key; fresh and upgrade tests verify it.
- `wf_definition_t` gains `source_revision`, and the new
  `workflow_definition_grant_sync_t` stores the last applied grant revision
  and digest per definition (see **Saved definitions** and **Grants**).
- `workflow_claim_idempotency_v2` is added (see **Idempotency**).
- New tables: the definition version table, `workflow_tool_publication_t`,
  `workflow_publication_operation_t` and `workflow_tool_binding_decision_t`.
- Every existing binding row becomes `legacy` with `active = false`. A Tool
  serves again only after it is republished.
- `workflow_action_authority_t` gains `credential_kind` and
  `workflow_run_credential_t` is added (see **Run credential selection**).
- The idempotency replay trigger reads the pinned revision's policy.

## Workflow Invoke

### Callers and routing

| Caller | Operation | Admission profile |
|--------|-----------|-------------------|
| Workflow Editor, scheduler, API client | `workflow_start` | `PortalExecution` |
| Gateway, executing a workflow-backed Tool for any MCP client, including Agents | `workflow_invoke` | `WorkflowBacked` |

### Identity and user token

`workflow_invoke` receives the same two credentials as `workflow_start`: the
original user token in `authorization`, and the Gateway's token in
`x-scope-token`. Workflow authenticates them with the existing `authenticate`
path in `rule_api.rs`: both JWTs must verify, and the scope token's service
id must be in `workflow.invocation.allowedCallerServiceIds`.

Unlike `workflow_start`, there is no light-oauth registration and no token
exchange. The run uses the original user token for its outbound calls.

Token lifetime is a configuration matter. The Gateway renews user tokens
before they expire (`renew_before_seconds` in `spa_auth`), and JWT
verification allows `clock_skew_in_seconds` after `exp` (`security.yml`).
Operators set these so that a token accepted at admission lasts a sync run.
Workflow does not add a check of its own. If a token stops verifying during
a run, the next outbound call fails with an authentication error.

### Run credential selection

This rule applies to every run, including async runs started with
`workflow_start`. For each outbound HTTP or MCP call, the task executor picks
the user token like this:

1. Use the original user token while it verifies and is more than
   `runCredential.originalTokenMarginSeconds` before its `exp`.
2. Otherwise, if the run has a LONG registration, use the token exchanged
   through light-oauth (`LongAuthority::token_for`).
3. Otherwise use the original token while it still verifies, and fail the
   call with an authentication error once it does not.

The margin only decides when a LONG run switches to the exchanged token. Only
`workflow_start` runs register with LONG, because only they can outlive the
original token. A `workflow_invoke` run never does.

The same selector serves bound MCP and protected executor HTTP calls.
An expired Invoke credential fails the outbound call. Retired broker rows
have no credential source and fail closed.

For registered protected executor HTTP calls, the target rule follows the
selected token. A LONG run using its still-valid original user token may call
the approved endpoint directly. Once the selector exchanges that token, the
HTTP target must use the configured Gateway origin. A definition whose
approved endpoint remains direct needs a Gateway route and revised target to
keep working after the exchange margin is reached.

**Where the original token is kept.** A run can move to another Workflow
replica, so the token must be stored, not only held in memory.

- `workflow_start` runs already store it in `workflow_long_credential_t` as
  part of the LONG registration.
- `workflow_invoke` runs store it sealed in `workflow_run_credential_t`,
  expiring at `deadline_ts`. It is deleted when the run reaches a terminal
  state, and a sweeper deletes any that outlive their expiry.

The token is sealed before the admission transaction starts. If no vault key
is configured, `workflow_invoke` is rejected: the run credential store never
writes plaintext. (The LONG store currently writes key id `plaintext` when no
keys are configured. Those rows are unchanged by this work; the keyring is
configured in the local Compose stack.)

**Admission hook.** Inside the `start_invocation_with_stage` transaction:

- on **Accepted**: take the per-Tool advisory lock, run the admission-limit
  counts, then insert the sealed credential and the action authority row with
  `credential_kind = 'invoke'`;
- on an **in-flight replay** (re-attach): replace the stored credential only
  if the row exists and the new token's `exp` is later;
- on a **terminal replay**: write nothing.

**Action authority without a grant.** The permit ledger that links nested
calls (`workflow_action_authority_t`) requires a non-nil `grant_id`, which
today comes from the LONG registration. For `workflow_invoke` runs the
authority row uses the run id as `grant_id`, the run's `deadline_ts` and an
action limit from `runtimeBounds`. `credential_kind` records where the run's
token comes from: `invoke`, `long` (set by the LONG producer) or `broker`
(existing rows without a LONG credential). The migration backfills existing
rows and then drops the column default, so every new writer must set it.

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

`workflow_invoke` is not ACL-checked against user roles at the Gateway,
because no client calls it. Workflow authorizes it in three layers:

1. **Caller.** The `x-scope-token` service id must be in
   `allowedCallerServiceIds`, as for `workflow_start`. The Gateway does not
   list `workflow_invoke` in `tools/list` and refuses it as a direct client
   `tools/call`. The manifest permission `workflow.instance.invoke` is a
   catalog label; there is no separate scope check.
2. **Binding.** The Tool must have an active, owner-approved revision, and
   its definition version must be active.
3. **Caller policy.** If the revision has a `callerPolicy`, the user token
   must satisfy it, for example `{"anyRole": ["claims-agent", "genai-admin"]}`.

Root calls are admitted with no broker, peer or policy, the same as
`workflow_start`. Workflow cannot tell a Portal call from a direct client
call by the scope token alone, because the Gateway sends its own token in
both cases; that is why the Gateway refusal in layer 1 matters.

The caller policy exists because the Tool ACL lives in the Portal and the
Tool owner can widen it later without new approval. The policy is part of the
approval digest, so the workflow owner can pin who may reach the workflow,
and Workflow enforces it on every call. An empty policy means the owner
accepts the Tool ACL. The review form shows the Tool's current ACL next to
the caller policy.

### Input

```json
{
  "stableToolRef": "0198a3c2-...",
  "expectedBindingDigest": "sha256:...",
  "expectedDefinitionDigest": "sha256:...",
  "input": { "claimId": "C-1001" },
  "idempotencyKey": "optional; required when the revision's key kind is explicit or business"
}
```

The Gateway sends no deadline, budget, class or depth. Workflow derives them
from the revision. `parentActionId` is reserved for nested calls and is
refused for now (see **Parent action**).

### Trust split

| Value | Source |
|-------|--------|
| Tool identity, input, explicit idempotency key | Gateway arguments, after the Gateway ACL and schema checks |
| Binding digest, definition digest | Gateway arguments, used only as pins that must match |
| Caller identity | User token and Gateway scope token, verified by Workflow |
| Mode, wait, deadline, class, budget, delegation, idempotency policy, cancellation, limits | Workflow binding revision |

### Admission

Workflow must:

1. Load the active revision for `stableToolRef` and the caller's host.
2. Compare `expectedBindingDigest` and `expectedDefinitionDigest` to it. On
   mismatch, return `WORKFLOW_DEFINITION_MISMATCH`. This detects a Gateway
   snapshot that is ahead of or behind Workflow.
3. Refuse a retired definition version with `WORKFLOW_DEFINITION_RETIRED`.
4. Check `callerPolicy`.
5. Look up the idempotency key. An active key attaches to the running
   instance, replays its result, or conflicts, before any capacity check (see
   **Idempotency**).
6. Build the `StartInvocationRequest` from the revision:
   - `mode` from `invocation_mode`, `execution_class` from the revision
   - `deadline_ts` = now + `total_deadline_ms`
   - `budget` from `runtime_bounds`
   - `permit_depth` 0
   - `cancellation_policy` from the revision
   - `binding_id` = the revision id
7. Seal the user token, then admit through `start_invocation_with_stage` with
   `AdmissionProfile::WorkflowBacked`, so `verify_binding`, orchestration and
   dependency validation, approval evidence and
   `enforce_deadline_aware_admission` all run. The admission hook checks
   `admissionLimits` and stores the credential and authority.

### Idempotency

The key scope is:

```text
scope_digest = canonical_sha256({ v: 1, kind, hostId, toolId,
                                  endUserSubject, key })
```

For `kind: derived`, `key` is the normalized input digest and Workflow
computes it; the Gateway no longer does. For `explicit` and `business`,
`key` is the caller's `idempotencyKey`, 1–256 characters.

The scope uses the end-user subject only, not the Gateway client. The key is
there to stop one user from starting duplicate runs of the same Tool with the
same input. `principal_subject` is still stored as the client principal,
because status and list ownership use it. The existing claim function
compares it, so the same user and key through a different client returns
`WORKFLOW_IDEMPOTENCY_CONFLICT` while the key is active. That is the safe
answer for writes.

`workflow_invoke` uses a new claim function,
`workflow_claim_idempotency_v2`; `workflow_start` keeps v1. "Same" below means
the same Tool, client principal, end user, definition and input:

| Existing reservation | Outcome |
| --- | --- |
| none | accept |
| run not terminal, same | replay: attach to the run |
| run not terminal, different | `WORKFLOW_IDEMPOTENCY_CONFLICT` |
| run terminal, inside `result_replay_until`, same | replay the stored result |
| run terminal, inside `result_replay_until`, different | `WORKFLOW_IDEMPOTENCY_CONFLICT` |
| run terminal, window expired | retire the old reservation and accept a new one |

A nonterminal run is never re-accepted, even after its window has passed,
so an expired window can never start a second run beside a live one. After
the window expires on a terminal run, a different client or definition gets
a new run, not a conflict.

Windows:

- At admission, `in_flight_until` and `result_replay_until` are both
  `deadline_ts`.
- When the run ends, `result_replay_until` becomes the terminal time plus the
  revision's `resultReplayMs`, except that it is the terminal time for a
  `FAILED` or `CANCELLED` run whose `effect_state` is `none`, so a clean
  failure can be retried at once.

With the read-only default `resultReplayMs: 0`, an identical call while the
first run is in flight attaches to it, and a call after completion starts a
new run. A write Tool must set `resultReplayMs` of at least 600000 for every
key kind, so a client retry within ten minutes gets the stored result instead
of a second write. A different input under the same explicit or business key
returns `WORKFLOW_IDEMPOTENCY_CONFLICT`.

The attach check runs before admission, so it skips capacity checks and keeps
the original deadline. Two identical calls that arrive together may both miss
it; the loser can get `WORKFLOW_CAPACITY_EXHAUSTED`, and its retry attaches.

### Waiting, timeout and re-attach

Workflow waits for the admitted run for
`min(sync_wait_ms, deadline_ts - now)`. The Gateway sets its HTTP timeout to
the revision's `syncWaitMs` from `mcp-router.yml` plus a 2 second margin, and
treats a transport timeout as `WORKFLOW_INVOCATION_UNAVAILABLE`.

When the wait ends without output:

- If the run can still finish before `deadline_ts`, Workflow returns
  `WORKFLOW_TIMEOUT` with the `workflowInstanceId` and `retryable: true`.
  The run continues. A retry with the same input maps to the same key and
  re-attaches to the running instance instead of starting another one.
- If `deadline_ts` has passed, Workflow cancels the run according to the
  revision's cancellation policy and returns `WORKFLOW_TIMEOUT` with
  `retryable: false`.

With the default `before-effects-only` policy, a write that has already been
dispatched is not cancelled; the run is left to finish and its result is
kept for the replay window. Workflow does not cancel a run merely because
the caller stopped waiting.

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

### Error envelopes

There are two kinds of failure result:

- **Authentication failure** (HTTP 401: a token is missing or does not
  verify) is a JSON-RPC error, as today.
- **Everything else** — an authenticated call refused by policy (today's
  403), an admission refusal, a timeout or a failed run — is a normal MCP
  result with `isError: true` and `structuredContent`:

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

`status` is `failed`, `timeout`, `cancelled` or `rejected`.
`workflowInstanceId` is present once a run exists. `retryAfterMs` is added
for capacity errors. `details` is an optional object with code-specific
data, passed through unchanged by the Gateway and the Portal client; the
equal-revision synchronization conflict uses it for Workflow's stored
`appliedRevision` and digest. `afterEffect` is true when the run's
`effect_state` is not `none`. The text content is `"<code>: <message>"`. For a failed run,
`error` is the run's stored normalized error, not a generic message.

`mcp_api::handler_error` and `ApiError` change accordingly: they currently
map 401 and 403 to JSON-RPC `-32001` and wrap other failures in the detail
text.

### Error mapping

| Condition | Code | Retryable |
|-----------|------|-----------|
| Input fails the definition schema | `WORKFLOW_INPUT_INVALID` | no |
| No active revision, or admission refused | `WORKFLOW_START_REJECTED` | no |
| Binding or definition digest mismatch | `WORKFLOW_DEFINITION_MISMATCH` | no, until the snapshots converge |
| Definition version retired | `WORKFLOW_DEFINITION_RETIRED` | no |
| Policy refused, invalid user or Gateway identity, revoked binding, or `parentActionId` supplied | `WORKFLOW_POLICY_DENIED` | no |
| User fails the revision's caller policy | `WORKFLOW_POLICY_DENIED` | no |
| Interactive capacity full, or a concurrency or rate limit hit | `WORKFLOW_CAPACITY_EXHAUSTED`, with `retryAfterMs` | yes |
| Same key, different input or different client | `WORKFLOW_IDEMPOTENCY_CONFLICT` | no |
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
5. **Workflow verifies the parent** (`rule_api.rs`, `receiver_parent`),
   through the verified dual-identity peer path and the action settings.

The #415 checkpoint stopped wiring the mTLS action dispatch in production
(`load_mcp_router_runtime`), so step 3 cannot run. Until it is restored,
Workflow refuses any `workflow_invoke` with `parentActionId` with
`WORKFLOW_POLICY_DENIED` "nested workflow-backed Tool calls require the
workflow action dispatch", and never downgrades it to a root start. Nested
workflow-backed Tools are deferred work, with their own design for deriving
the child's depth, class, deadline and budget from the parent permit.

The purpose is containment. Without the link, a workflow could call a Tool
that starts another workflow, which calls a Tool that starts another, each
with a fresh 30-second deadline and a full budget.

## Gateway Changes

- `execute_workflow_tool` calls `workflow_invoke` instead of `workflow_start`
  followed by `wait_for_workflow_start_result`. The wait loop is removed.
- `McpWorkflowBindingConfig` shrinks to what the Gateway uses: Tool name,
  `stableToolRef`, input schema, `workflowDefinitionId`, `workflowVersion`,
  `definitionDigest`, `bindingDigest`, `invocationMode`, `syncWaitMs` and
  `resultTextMode`. `bindingDigest` is optional in the parser; a Tool
  without it is refused with `WORKFLOW_START_REJECTED` "republish this Tool",
  and the other Tools keep working. The Gateway keeps the request-size
  pre-check from `runtimeBounds.maximumRequestBytes` as an early rejection;
  Workflow still enforces the full budget.
- A direct `workflow_start` success proves the definition can start; it does
  not publish a workflow-backed Tool revision. The Tool must first receive an
  approved `workflow_binding_publish` revision, then a Gateway Tool publication
  containing its `bindingDigest` must be activated. The Tool page disables
  Invoke while Portal reports that its binding needs publication. A confirmed
  Gateway error with `afterEffect=false` is shown as a rejection, separate
  from the warning for an unconfirmed network outcome.
- The Workflow Definition list's binding count is the count awaiting owner
  review, not the number of Portal source bindings. Its owner binding page
  lists requested Workflow revisions; a legacy binding with no request time
  does not appear there until a binding publication succeeds.
- Legacy Portal source bindings can carry `inFlightDedupMs` and a zero
  `maximumCostUnits`; neither is accepted by the current binding publication
  contract. Portal normalizes these values when building a new publication
  request. A D21 operation already stored with legacy arguments is immutable:
  repeating Retry cannot change its payload. Such a pending operation needs
  explicit outcome reconciliation before a new operation for the same binding
  can be submitted.
- The Gateway no longer computes the idempotency key.
- Workflow `isError` results and their `structuredContent` pass through as
  described in **Error envelopes**.
- The Gateway forwards the verified user bearer and supplies its own app bearer
  in `X-Scope-Token` on calls to Workflow.
- `workflow_invoke` is hidden from `tools/list` and refused as a client
  `tools/call`.
- `loadRuntimeWorkflowTool` in `ConfigPersistenceImpl` takes the digests from
  the Workflow receipts instead of recomputing them.

## Other Contract Changes

- **`cancellation_policy`, `admission_limits`, `caller_policy`,
  `tool_annotations`.** Added to the normalizer, the Portal binding table and
  the Workflow revision.
- **Portal publication columns.** The Portal binding row stores
  `publication_status`, `published_binding_digest`,
  `published_definition_digest`, `published_revision_id`,
  `published_aggregate_version`, `published_request_digest`,
  `publication_comment`, `publication_status_ts` and
  `publication_decided_by`; `gateway_tool_binding_t` stores
  `binding_digest`.
- **Portal synchronization tables.** `workflow_sync_state_t` holds the
  desired revision and actor and the acknowledged revision and digest per
  definition and kind (see **Synchronization**). `scheduler_lock_t` gets a
  seed row for lock id 2. `workflow_operation_t` is the operation ledger for
  every other mutating Workflow call (see **Lost receipts**). Both are
  excluded from snapshots, like `outbox_message_t`.
- **`workflow_wait_result`.** Removed from the Gateway path. It stays for
  `workflow_start` callers, but must return the stored normalized error for
  failed and cancelled runs instead of null, and gets its `examples.json`
  entry so `validate-contracts.mjs` passes.
- **`workflow_start`.** Its semantics do not change. It stays async with the
  `PortalExecution` profile, gains the optional `expectedDefinitionDigest`,
  and rejects a caller-supplied `stableToolRef`, so a Tool call cannot bypass
  binding admission through it.

## Implementation Plan

The step-by-step plan, with gates and schema, is kept outside this repository
in the workflow-invoke implementation plan. In outline:

1. **Workflow schema and contracts.** Migration `0018`, manifest entries,
   schemas and examples, error envelopes.
2. **Workflow publication.** Publisher assertion, definition save, publish
   and retire, grants sync, binding revisions, digests, effect matrix, task
   evidence, pinned readers, owner approval, carry-over, decisions and
   concurrency.
3. **Portal.** Database patch, normalizer, Workflow client with the acting user
   token, save-then-start, per-Tool publication, preview digest, approval
   commands and queries, portal-view screens.
4. **`workflow_invoke`.** Admission, sealed run credential, admission limits,
   authority, run credential selection, broker removal, wait, result and
   envelopes.
5. **Gateway.** Route to `workflow_invoke`, shrink the binding config, pass
   errors through, forward the user token, supply Gateway's app token, hide `workflow_invoke`.
6. **Cleanup.** `workflow_start` hardening, FDW sync removal, documentation,
   light-portal-test, local Compose image tag, user runbook.

## Qualification Checklist

- A second identical sync call after completion starts a new run when
  `resultReplayMs` is 0, and replays within a positive window.
- A retry after `WORKFLOW_TIMEOUT` re-attaches to the same instance.
- A clean `FAILED` run can be retried at once; a failure after an effect
  replays for the window.
- A run past `deadline_ts` is cancelled before effects and reports
  `WORKFLOW_TIMEOUT`.
- A failed task returns `WORKFLOW_TASK_FAILED` with the stored message on the
  first call, not after the Gateway timeout.
- A `workflow_invoke` with `parentActionId` is refused with
  `WORKFLOW_POLICY_DENIED`.
- A Gateway snapshot with a stale `bindingDigest` gets
  `WORKFLOW_DEFINITION_MISMATCH`; one without `bindingDigest` gets
  `WORKFLOW_START_REJECTED` for that Tool only.
- A publication call without a valid user `Authorization` or Gateway
  `X-Scope-Token` is refused before storage access. A mismatched host is refused.
- A delayed save or grant set with a lower `sourceRevision` returns `stale`
  and changes nothing.
- With Workflow stopped, a grant revocation shows "Sync pending" and the Portal
  and Workflow revision gap. Once Workflow returns, a user presses **Sync now**;
  subsequent dispatches then honor the acknowledged revocation.
- A publish whose receipt is lost is shown as unconfirmed; a retry by the
  same user with the same `operationId` returns the stored receipt and does
  not overwrite a newer Portal projection. Another user cannot retry it.
- When revision N+1 is acknowledged before a delayed receipt for N, the
  Portal keeps N+1 and its digest.
- An expired idempotency window on a terminal run starts a new run; a
  nonterminal run is never re-accepted.
- Publishing the same payload twice returns `unchanged`. Reusing an
  `operationId` with a different payload is a conflict. Publishing a new
  digest for a published version is rejected.
- A run admitted before a republish or revocation finishes with its pinned
  revision.
- A retired definition version refuses new runs and cannot be retired while
  an active revision pins it.
- Editing a definition in the Portal and starting it, from the definition
  list or the editor, runs the edited text; a stale head returns
  `WORKFLOW_DEFINITION_MISMATCH`.
- An acknowledged grant revocation stops the next call of a running
  workflow that needs it.
- A failed binding publish leaves the previous Gateway entry for that Tool
  active, and the other Tools in the batch are published.
- The local stack works with `workflow-projection-sync` removed and no FDW
  server configured.
- `validate-contracts.mjs` passes.
- A revision published by someone other than the definition owner is
  `pendingApproval`, is left out of the Gateway snapshot, and becomes active
  only after the owner approves the reviewed digest.
- A name-only change keeps the approved revision active. A deadline change
  makes a new pending revision while the previous one keeps serving.
- A revoked revision returns `WORKFLOW_POLICY_DENIED` on the next call.
- A user with `genai-admin` but not `workflow-admin` can call a
  workflow-backed Tool, and a user outside a revision's caller policy cannot.
- `workflow_invoke` makes no light-oauth call. The run's outbound calls carry
  the original user token, and the stored token is gone after the run ends.
- `workflow_invoke` is rejected when no vault key is configured.
- A `workflow_start` run whose original token is still valid makes outbound
  calls with that token and does not call light-oauth. Once the token is
  within the margin, it switches to the exchanged token.
- No outbound call uses the retired grant broker.
- A binding with `invocationMode: async` or a human task is rejected.
- A write Tool with `resultReplayMs` below 600000 is rejected. A read-only
  Tool reaching a write is rejected. A write Tool with evidence is admitted.
- Concurrent and per-minute limits return `WORKFLOW_CAPACITY_EXHAUSTED` with
  `retryAfterMs`, re-attaches do not count, and parallel admissions cannot
  exceed the cap.
- Re-pinning to a new version with unchanged fields stays active; a version
  that exceeds `maximumParallelism` is rejected at publish; a version
  published with `reapprove` sends the revision to the owner.
- A client `tools/call` for `workflow_invoke` is refused by the Gateway.

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

With mTLS, Workflow can also tell the Gateway apart from other callers by
peer identity, and nested calls can use the restored action dispatch. The
`workflow_invoke` and publication contracts do not change.

## Decisions

1. Publication goes through the Gateway, which connects to Workflow. Per-host
   Workflow discovery through controller-rs comes later and does not change
   the contract.
2. Publication is not a workflow.
3. Nested workflow-backed Tool calls are refused until the mTLS action
   dispatch is restored; they are designed separately.
4. Admitted runs keep their binding revision. Grants are read live.
5. The idempotency scope uses the end-user subject. The same user through a
   different client conflicts while the key is active.
6. The Gateway is the only caller of `workflow_invoke`. Agents use the
   workflow-backed Tool. Root calls use the existing caller check; there is no
   separate `workflow.instance.invoke` scope check.
7. The workflow owner approves revisions from other Tool owners. The pending
   revision and the decision live in Workflow; the UI is in portal-view.
8. Bindings carry concurrency and rate limits that the owner approves.
9. Approval carries over to a new definition version when nothing it covers
   changed, the owner is unchanged and the version fits the approved limits.
10. Write workflows can be Tools, under the effect matrix and a replay window
    of at least ten minutes.
11. Workflow-backed Tools are sync only. `workflow_invoke` passes the
    original user token and the Gateway token. There is no light-oauth
    registration or token exchange. Token lifetime is set by configuration.
12. Every run uses the original user token until it is near expiry, then the
    LONG exchanged token if the run has one. The grant broker is removed.
13. The proposed `admissionLimits` defaults are accepted.
14. Tool permission is enough to call a workflow-backed Tool. The workflow
    owner can narrow it with the revision's caller policy.
15. Gateway-to-Workflow mTLS is future work (see **Future: Gateway to Workflow
    mTLS**).
16. Portal-authoritative publication carries the acting user's bearer in
    `Authorization`. Gateway verifies the user, forwards that bearer and adds
    Gateway's application bearer in `X-Scope-Token` for Workflow to verify.
17. Portal keeps Workflow's saved definition head in step with
    `workflow_definition_save` through durable, revisioned delivery.
    `StartWorkflow`, including the editor's Start, runs only the
    acknowledged revision.
18. Grants belong to the definition and are replaced with
    `workflow_definition_grants_sync`. A revocation is enforced once Workflow
    acknowledges it; dispatched operations are not undone.
19. Every published binding is an immutable Workflow revision; Workflow
    generates task evidence on approval.
20. The Portal records every other mutating Workflow call in an operation
    ledger. A lost receipt is recovered only by the requesting user resending
    the same request with the same `operationId`; there is no unattended
    recovery.

## Open Questions

None at present.
