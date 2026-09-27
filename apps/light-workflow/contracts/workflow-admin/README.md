# Workflow admin contracts

These Phase 0 contracts freeze the public Gateway and private Workflow tool shapes before handler or transport work. Gateway and Workflow expose the same names and schemas. Identity and Host come only from the trusted invocation context, never from tool arguments.

Run `npm ci && npm test` in this directory. AJV validates the complete draft 2020-12 schemas and examples. Custom checks then enforce the manifest, stable error registry, identifier separation, bounded pagination, mutation concurrency, and the absence of identity/fencing arguments after resolving schema references.

The parent test also validates `proposed-runtime-v1/`. Step 08 promotes native process deletion, safe task detail, append-only notes, and bounded note reads into the active manifest. The proposed package now excludes process-only legacy list/detail tools under the revised migration scope. Gateway publication and live authorization are later gates; an active Workflow manifest entry alone is not a Gateway publication.

Step 09 adds `gateway-publication.json` as the exact additive native Tool and restricted-route inventory. `node build-gateway-publication.mjs` generates `gateway-tools-list.json`, an MCP `tools/list` style import source with bundled schemas for the six new native Tools. Register it as a Portal MCP API version with Workflow service `com.networknt.workflow-1.0.0` and streamable HTTP transport path `/mcp`; use the existing Gateway Tool preview/publish command to stage only the selected Tools and explicit ACLs. This JSON is source metadata, not a Portal CloudEvent or an active Gateway snapshot. The local config delta must be generated through `event-generation`, imported by the operator, promoted as a snapshot, and reloaded before runtime qualification. Do not publish the separate Phase 6 audit Tool or remove old commands in Step 09.

Contract version `0.3.0-definition-start` removes the workflow-backed Tool binding from native `workflow_start`. Native callers provide the saved definition ID, object input and idempotency key; Workflow derives execution policy from the saved definition and runtime configuration. Workflow-backed Tool calls use this same start operation and may supply `expectedDefinitionDigest` so the active saved definition must still match the published Tool binding. A successful human-task mutation reports that its completion was durably recorded; asynchronous executor continuation remains observable through process/task reads. Changing names, required fields, identifier meanings, error codes, or bounds requires a reviewed contract version and updated consumers/fixtures.

`workflow_wait_result` returns its documented output for completed and still running instances. Failed and cancelled instances return an MCP `isError` result whose `structuredContent` follows `WorkflowErrorResult`; the examples include both terminal states. `workflow_start` rejects `stableToolRef` with `WORKFLOW_INPUT_INVALID`; use the workflow-backed Tool for binding-based invocation. `workflow_start` remains asynchronous and retains its v1 unlimited result replay window.


### Workflow error results

`WorkflowErrorResult` in `schemas.json#/$defs/WorkflowErrorResult` is the structured content schema for every Workflow `tools/call` result with `isError: true`. It applies to native tools, publication tools, and the gateway-internal `workflow_invoke` contract. Optional `details` is an object and is omitted when absent.
