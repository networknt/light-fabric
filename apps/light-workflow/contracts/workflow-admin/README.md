# Workflow admin contracts

These Phase 0 contracts freeze the public Gateway and private Workflow tool shapes before handler or transport work. Gateway and Workflow expose the same names and schemas. Identity and Host come only from the trusted invocation context, never from tool arguments.

Run `npm ci && npm test` in this directory. AJV validates the complete draft 2020-12 schemas and examples. Custom checks then enforce the Tool catalog, stable error registry, identifier separation, bounded pagination, mutation concurrency, and the absence of identity/fencing arguments after resolving schema references.

The parent test also validates `proposed-runtime-v1/`. Step 08 promotes native process deletion, safe task detail, append-only notes, and bounded note reads into the active catalog. The proposed package now excludes process-only legacy list/detail tools under the revised migration scope. Gateway publication and live authorization are later gates; a Workflow catalog entry alone is not a Gateway publication.

`workflow-tools-list-full.json` is the single Tool catalog and Portal MCP `tools/list` import source. Its `tools` array contains all 35 Workflow MCP Tools with bundled schemas, including `workflow_invoke`. That Tool is available on Workflow MCP but has `gatewayPublication: false` and must not be exposed through light-gateway. Contract metadata and the original schema references are retained on each Tool; `schemas.json` holds the shared schema definitions. Register the `tools` array as a Portal MCP API version with Workflow service `com.networknt.workflow-1.0.0` and streamable HTTP transport path `/mcp`. `gateway-publication.json` selects the additive Gateway publication subset and restricted routes; it is not another Tool catalog. Use the Gateway Tool preview/publish command to stage only selected Tools and explicit ACLs. The JSON is source metadata, not a Portal CloudEvent or active Gateway snapshot.

When a Tool input schema changes, first update the Portal MCP API version from
this catalog (or rediscover the rebuilt Workflow server). Verify the affected
Portal API endpoint's `tool_schema` contains the change. Then preview and
publish the Gateway Tools and activate the resulting config snapshot. Gateway
publication compiles from the Portal endpoint schema; republishing without the
API version update preserves the previous schema.

Contract version `0.3.0-definition-start` removes the workflow-backed Tool binding from native `workflow_start`. Native callers provide the saved definition ID, object input and idempotency key; Workflow derives execution policy from the saved definition and runtime configuration. Workflow-backed Tool calls use this same start operation and may supply `expectedDefinitionDigest` so the active saved definition must still match the published Tool binding. A successful human-task mutation reports that its completion was durably recorded; asynchronous executor continuation remains observable through process/task reads. Changing names, required fields, identifier meanings, error codes, or bounds requires a reviewed contract version and updated consumers/fixtures.

`workflow_wait_result` returns its documented output for completed and still running instances. Failed and cancelled instances return an MCP `isError` result whose `structuredContent` follows `WorkflowErrorResult`; the examples include both terminal states. `workflow_start` rejects `stableToolRef` with `WORKFLOW_INPUT_INVALID`; use the workflow-backed Tool for binding-based invocation. `workflow_start` remains asynchronous and retains its v1 unlimited result replay window.

Binding publish/retire may return `WORKFLOW_INPUT_INVALID` with `afterEffect=false` and
`details.requestValidation` when an authenticated, serialized operation has a
negative `expectedAggregateVersion`. Example evidence:
`{"version":1,"discriminator":"negativeExpectedAggregateVersion","operationId":"11111111-1111-4111-8111-111111111111","toolName":"workflow_binding_publish"}`.
The same operation ID/request replays this stored rejection. Unproven input errors
do not carry this marker. Portal verifies the Workflow Tool origin, marker,
operation/tool correspondence, and its exact stored request before treating the
error as definitive.

Binding publish also uses finite version 2 evidence after its serialized
operation check: `bindingFields` with `requestSection:"binding"`, or
`bindingReach` with `requestSection:"reach"` (the typed `dependencies` and
`endpointTargets` arrays together). Both require `operationId` and
`toolName:"workflow_binding_publish"`. These cover explicit deterministic
validation of supplied binding/reach fields. Portal verifies the corresponding
section and type in its stored exact request, a nonnegative expected version,
Workflow Tool origin, `WORKFLOW_INPUT_INVALID`, and `afterEffect=false`.
Additional evidence members are rejected by the schema and remain unknown to
Portal.
Serialization, digest computation, configuration, database, and live-state
errors have no version 2 evidence and remain unconfirmed.


### Workflow error results

`WorkflowErrorResult` in `schemas.json#/$defs/WorkflowErrorResult` is the structured content schema for every Workflow `tools/call` result with `isError: true`. It applies to native tools, publication tools, and `workflow_invoke`. Optional `details` is an object and is omitted when absent.
