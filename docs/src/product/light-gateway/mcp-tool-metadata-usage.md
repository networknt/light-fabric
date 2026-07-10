# MCP Tool Metadata Usage

This document describes how `light-gateway` MCP tool metadata should be used
for tool search, progressive disclosure, deterministic routing, policy
enforcement, and diagnostics.

The main principle is simple: metadata can help an agent find the right tool,
but `tools/call` remains the execution and authorization boundary.

## Background

The MCP router can expose tools backed by downstream MCP servers and tools
backed by OpenAPI endpoints through the same gateway MCP endpoint.

A downstream MCP tool can be represented as:

```yaml
- name: local_mcp_echo
  path: /mcp
  method: call
  apiType: mcp
  endpoint: echo@call
  endpointName: echo
  protocol: http
  productId: gtw
  serviceId: com.networknt.local.mcp-1.0.0
  endpointId: 019ec75c-72c5-702e-8e42-59dcf1e68cc2
  description: Echoes back the input
  inputSchema:
    type: object
    properties:
      message:
        type: string
    required:
      - message
  toolMetadata:
    routing:
      domain: MCP0002
      semanticNamespace: MCP0002
      semanticDescription: Echoes back the input
      semanticKeywords:
        - echo
        - Echoes back the input
      semanticWeight: 1.0
      sensitivityTier: internal
      sourceProtocol: mcp
    safety:
      read_only: false
      idempotent: false
      destructive: false
      humanApprovalRequired: false
    lifecycle:
      version: 1.0.0
      status: active
    read_only: false
    destructive: false
```

An OpenAPI-backed tool can be represented as:

```yaml
- name: demo_customer_profile_api_get_customer_preferences
  path: /customers/{customerId}/preferences
  envTag: dev
  method: get
  apiType: openapi
  endpoint: /customers/{customerId}/preferences@get
  protocol: http
  productId: gtw
  serviceId: com.networknt.customer.profile-1.0.0
  endpointId: 019e621b-3a4c-78f4-82f5-16ed24f5ba58
  description: Get customer preferences
  inputSchema:
    type: object
    properties:
      customerId:
        type: string
        description: Customer identifier.
      channel:
        type: string
        default: portal
        description: Requested channel context.
    required:
      - customerId
  toolMetadata:
    routing:
      domain: Customers
      semanticNamespace: API0004
      semanticDescription: Get customer preferences
      semanticKeywords:
        - Customers
        - getCustomerPreferences
        - Get customer preferences
      semanticWeight: 1.0
      sensitivityTier: internal
      sourceProtocol: openapi
      parameters:
        customerId: path
        channel: query
    safety:
      read_only: true
      idempotent: true
      destructive: false
      humanApprovalRequired: false
    runtime:
      cacheTtlSeconds: 60
      costTier: low
      estimatedLatencyMs: 100
    lifecycle:
      version: 1.0.0
      status: active
    read_only: true
    destructive: false
```

Imported catalog data may store `inputSchema` and `toolMetadata` as escaped JSON
strings. The router accepts that shape, but hand-authored config should prefer
structured YAML or JSON. Structured metadata is easier to validate, diff, index,
and review.

## Current Runtime Boundary

At runtime, the MCP tool config includes:

| Field | Purpose |
|-------|---------|
| `name` | Gateway-facing tool name exposed to agents. |
| `endpointName` | Backend MCP operation name used when forwarding `tools/call` to a downstream MCP server. |
| `description` | Human and model-facing summary. |
| `protocol` | Discovery and direct-registry protocol selector. |
| `serviceId` | Service identity used for portal-registry or direct-registry lookup. |
| `envTag` | Optional environment discriminator for service lookup. |
| `targetHost` | Direct base URL override. |
| `path` | HTTP path or MCP endpoint path. |
| `method` | HTTP method, or `call` for backend MCP calls. |
| `endpoint` | Stable policy endpoint key, such as `echo@call` or `/offers@get`. |
| `apiType` | `mcp` or `openapi`. |
| `inputSchema` | JSON Schema used for model tool parameters and argument validation. |
| `toolMetadata` | Structured routing, semantic, safety, and governance metadata. |

The gateway `tools/list` response should stay compact. It exposes the fields
needed by model tool calling: `name`, `description`, and `inputSchema`.

Richer metadata belongs in the catalog/search layer and gateway runtime config.
This avoids flooding the model context with operational fields while still
making the data available for ranking, policy, routing, diagnostics, and audit.

## Metadata Responsibilities

Use metadata in three layers:

| Layer | Uses metadata for | Should not use metadata for |
|-------|-------------------|-----------------------------|
| Portal or catalog search | Ranking, assignment, filtering, disclosure, governance preview. | Direct backend execution. |
| Agent runtime | Progressive disclosure and per-turn schema selection. | Bypassing gateway policy. |
| `light-gateway` MCP router | Deterministic routing, argument mapping, access control, response filtering, audit, diagnostics. | Letting model text decide target URLs or service routing. |

The agent can use metadata to decide which tools to offer to the model. The
gateway uses config metadata to decide how an accepted `tools/call` is executed.

## Search And Progressive Disclosure

Agents should not send every configured tool to the model. Instead, they should
search the effective agent catalog, select a small set of likely tools, then
intersect that set with live gateway `tools/list`.

Recommended flow:

```text
user prompt
  -> load assigned effective agent catalog
  -> search metadata and schema text
  -> apply safety and policy disclosure filters
  -> select top tools for the turn
  -> intersect selected names with gateway tools/list
  -> send only selected schemas to the model
  -> execute selected tool through gateway tools/call
```

The search index should include:

| Metadata | Search use |
|----------|------------|
| `name` | Exact and alias matching. |
| `endpointName` | Backend operation matching. |
| `description` | General keyword matching. |
| `routing.semanticDescription` | Agent-oriented capability description. |
| `routing.semanticKeywords` | High-value domain and operation terms. |
| `routing.domain` | Business-domain filtering, such as `Customers` or `Offers`. |
| `routing.semanticNamespace` | Product, API, or catalog namespace filtering. |
| `routing.sourceProtocol` | Protocol-aware selection between MCP and OpenAPI tools. |
| `routing.sensitivityTier` | Disclosure and governance filtering. |
| `routing.semanticWeight` | Score multiplier for preferred or higher-quality tools. |
| `inputSchema.properties` | Parameter-intent matching, such as `customerId`, `state`, or `category`. |
| `inputSchema.required` | Completeness checks before exposing or calling a tool. |
| `safety.read_only` | Prefer safe read tools when the prompt is informational. |
| `safety.idempotent` | Decide whether retries are safe for identical arguments. |
| `safety.destructive` | Hide or require approval for destructive tools. |
| `safety.humanApprovalRequired` | Route to approval or workflow instead of direct call. |
| `runtime.costTier` | Prefer cheaper tools when multiple tools can satisfy the prompt. |
| `runtime.estimatedLatencyMs` | Prefer faster tools for interactive turns. |
| `lifecycle.status` | Prefer active tools and avoid deprecated or retired tools. |

The search result should be small. A practical default is 3 to 12 tools per
turn. Larger lists increase token use and can lead to the model choosing an
irrelevant tool.

Schema indexing should be bounded. For complex request bodies, index the
top-level property names and descriptions by default, then include nested
properties only when the importer marks them as semantically useful. Deeply
nested OpenAPI schemas can otherwise flood the index with low-value keywords
and increase false-positive tool matches. `semanticKeywords` should be the
curated override when schema text is noisy.

## Ranking

A simple first-pass ranking can combine keyword and structured metadata:

```text
score =
  keyword(name, endpointName, description)
  + 2.0 * keyword(routing.domain, routing.semanticNamespace)
  + 2.0 * keyword(routing.semanticKeywords)
  + 1.5 * keyword(inputSchema property names and descriptions)
  - cost penalty
  - latency penalty
  + skill priority
```

Then multiply by `routing.semanticWeight`, with a lower bound such as `0.1` to
avoid accidentally zeroing a valid tool:

```text
final_score = score * max(routing.semanticWeight, 0.1)
```

This can be implemented with plain keyword matching first. Later, the same
metadata can feed vector search or hybrid search:

- Use `semanticDescription`, `semanticKeywords`, `description`, schema property
  descriptions, tags, and categories for embeddings.
- Use `routing.domain`, `semanticNamespace`, `sourceProtocol`,
  `sensitivityTier`, `read_only`, `destructive`, and assignment state as
  structured filters.
- Use `endpointId` as the stable document ID for evaluation and feedback.
- Favor `lifecycle.status: active` over `deprecated`, and exclude `retired`
  tools from normal disclosure.
- Use `runtime.costTier`, `runtime.estimatedLatencyMs`, and rate-limit metadata
  as tie-breakers when several tools can satisfy the same intent.

## Disclosure Filters

Search ranking should run after coarse assignment and governance filters.

Before a tool schema is sent to the model, the agent or catalog API should
remove tools that are not appropriate for the current principal and task:

| Filter | Behavior |
|--------|----------|
| Agent assignment | Only include tools assigned through the effective agent catalog. |
| Environment | Match `hostId`, `serviceId`, and `envTag`. |
| Runtime availability | Intersect selected catalog names with live gateway `tools/list`. |
| Lifecycle | Hide retired tools and prefer active tools over deprecated tools. |
| Sensitivity | Do not disclose tools above the caller or agent sensitivity allowance. |
| Destructive flag | Hide unless an approval path or guarded workflow is configured. |
| Human approval | Route to approval or workflow instead of direct model execution. |
| Read-only preference | Prefer read-only tools unless the user intent requires mutation. |
| Budget and rate limit | Prefer lower-cost tools and avoid tools whose rate budget is exhausted. |

Disclosure is not authorization. A hidden tool should not be shown to the model,
but a visible tool must still be authorized by `tools/call`.

## Tools List

The gateway `tools/list` endpoint has two jobs:

1. Report which configured tools are currently executable through the gateway.
2. Optionally filter the list by access-control visibility.

It should not become the primary semantic search API. The catalog or agent cache
is a better place for richer semantic ranking because it can include skill
assignment, tags, categories, prompt instructions, feedback, and non-runtime
governance data.

The recommended pattern is:

```text
catalog search selects candidate tool names
gateway tools/list confirms executable visible tools
model receives only confirmed tool schemas
```

This keeps the runtime boundary clean:

- Catalog search can evolve independently.
- Gateway `tools/list` stays protocol-compatible and compact.
- Gateway `tools/call` remains the final enforcement point.

See [MCP Tools List Access Control](mcp-tools-list-access-control.md) for list
visibility filtering.

## Catalog Policy And Gateway Visibility

Catalog policy means the portal-side disclosure decision made before a tool is
shown to an agent. It includes agent assignment, skill-to-tool links,
environment, sensitivity tier, lifecycle status, approval requirements, and any
tenant or persona rules owned by the catalog/control plane.

Gateway visibility means the runtime decision made by `light-gateway`
`toolsListAccessControl` when `tools/list` is requested. It checks the current
token, claims, gateway policy, and live runtime configuration.

Both are needed:

```text
visible tools =
  assigned catalog tools
  intersect live gateway tools/list
  intersect gateway toolsListAccessControl result
```

Catalog policy prevents irrelevant or unassigned tools from reaching the model.
Gateway `toolsListAccessControl` prevents the agent from seeing tools that are
not visible to the current runtime principal. Neither replaces `tools/call`
authorization.

## Execution Routing

After the model chooses a tool, the agent calls:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "demo_customer_profile_api_get_customer_preferences",
    "arguments": {
      "customerId": "CUST-1001",
      "channel": "portal"
    }
  }
}
```

The gateway then resolves the configured tool by `name` and executes it
deterministically.

For `apiType: mcp`:

1. Resolve the backend target from `targetHost`, or from `serviceId`, `envTag`,
   and `protocol`.
2. Establish or reuse the backend MCP session.
3. Forward a backend `tools/call`.
4. Use `endpointName` as the backend tool name when present.
5. Return the backend MCP result to the caller after access-control response
   filtering.

For `apiType: openapi`:

1. Resolve the backend target from `targetHost`, or from `serviceId`, `envTag`,
   and `protocol`.
2. Start from configured `path` and `method`.
3. Read `toolMetadata.routing.parameters`.
4. Map arguments into path, query, header, cookie, or body.
5. Invoke the HTTP endpoint.
6. Convert the HTTP response into an MCP result.
7. Return the result after access-control response filtering.

Parameter mapping is what lets one model-facing argument object become a
correct HTTP request:

```yaml
toolMetadata:
  routing:
    parameters:
      customerId: path
      channel: query
      idempotency-key: header
      body: body
```

The model still submits one flat JSON argument object. The gateway owns the
split into HTTP destinations. A mapped argument should be consumed exactly once:
path, query, header, and cookie arguments should be stripped from fallback body
placement, and body arguments should not also become query parameters.

Header and cookie names should come from admin-approved catalog metadata, not
from model output. The gateway should validate those names and block
hop-by-hop or security-sensitive headers unless an explicit administrative
allowlist permits them.

If no mapping is present, the router falls back to method-based behavior:

- `GET` and `HEAD` arguments become query parameters.
- JSON-body methods send the argument object as the request body.

Path-template tools should not rely on fallback behavior. For a path such as
`/customers/{customerId}`, the OpenAPI import or catalog authoring process
should generate:

```yaml
toolMetadata:
  routing:
    parameters:
      customerId: path
```

Without that mapping, the gateway cannot safely know whether `customerId`
belongs in the path, query string, header, cookie, or body. Importers can infer
this from OpenAPI parameter locations, but the runtime config should carry the
resolved mapping explicitly.

The supported path placeholder syntax should be OpenAPI-style `{name}`. The
gateway should not infer Spring or Express-style `:name` segments by default;
importers should normalize or reject non-OpenAPI path-template syntax before
publishing gateway config.

## Retries, Caching, And Rate Limits

Safety and runtime metadata can help agents and gateways decide how aggressively
to retry or cache calls.

Recommended fields:

```yaml
toolMetadata:
  safety:
    read_only: true
    idempotent: true
    destructive: false
    humanApprovalRequired: false
  runtime:
    cacheTtlSeconds: 60
    retry:
      enabled: true
      maxAttempts: 2
      retryOn:
        - timeout
        - 502
        - 503
        - 504
    rateLimit:
      key: customer-profile-read
      costUnits: 1
    costTier: low
    estimatedLatencyMs: 100
```

`idempotent` is different from `read_only`. A read-only tool is normally
idempotent, but some write tools can also be idempotent when they use an
idempotency key. Automatic retries should require `idempotent: true` and an
explicit retry policy. Destructive tools should not be retried automatically
unless the operation is proven idempotent and protected by an idempotency key.

`cacheTtlSeconds` should only apply to results that are safe to cache for the
current caller. Cache keys must include the tool name, normalized arguments,
authenticated principal or tenant boundary, and any claims that affect
authorization or response filtering. Response-filtered results must not be
served across users with different visibility.

There are two distinct cache types:

| Cache | Purpose | Recommended owner |
|-------|---------|-------------------|
| `tools/list` visibility cache | Reuse the filtered list of visible tool names for the same principal, claims, headers, and query. | Gateway MCP router. |
| `tools/call` response cache | Reuse an actual tool result for identical safe calls. | Backend service first; gateway only when explicitly configured. |

The `tools/list` cache is a runtime optimization for discovery. It does not
cache business data and does not change `tools/call` authorization. It should be
enabled when `toolsListAccessControl` is enabled and bounded by a maximum entry
count.

Gateway-level `tools/call` response caching should be opt-in and conservative.
Start with backend-owned caching for expensive read APIs. Add gateway response
caching only for tools with `read_only: true`, `idempotent: true`, a positive
`cacheTtlSeconds`, and a cache key that includes all principal, tenant,
argument, environment, and response-filter dimensions.

Rate-limit and cost metadata should influence ranking and diagnostics. It can
also prevent an agent from repeatedly selecting a tool whose backend quota is
already exhausted.

## Service Resolution

Routing should avoid model-supplied URLs. The selected tool already carries the
deployment routing data.

Resolution order:

1. Use `targetHost` when explicitly configured.
2. Use direct-registry when a matching static URL is configured.
3. Use service discovery through `serviceId`, `envTag`, and `protocol`.

The gateway must validate protocol compatibility when direct-registry is used.
For example, a tool configured for `protocol: http` should not silently route to
an incompatible backend entry.

`targetHost` is administrative configuration, not model input. Automated imports
must treat `targetHost` as untrusted until the owning control plane validates
and approves it. Validation should include allowed schemes, allowed hostnames or
service identities, optional CIDR allowlists, DNS and redirect handling, and
environment ownership. This prevents a compromised catalog import from turning
the gateway into an SSRF path to metadata services, loopback addresses, or
internal control-plane endpoints.

DNS and CIDR checks must be enforced on the actual resolved address used by the
gateway connector, not only on the URL string. This prevents DNS rebinding from
turning an approved-looking hostname into a loopback, link-local, private, or
metadata-service address at connection time. Redirect targets should go through
the same validation.

## Access Control

MCP tool metadata should complement, not replace, access-control policy.

The gateway applies the shared access-control runtime around `tools/call`:

- `req-acc` runs before the downstream tool is invoked.
- `res-fil` runs after the downstream result is converted to an MCP result.

Use metadata as follows:

| Metadata | Access-control use |
|----------|--------------------|
| `endpoint` | Stable key for endpoint rules. |
| `endpointId` | Stable audit and governance identifier. |
| `sensitivityTier` | Disclosure and policy input. |
| `read_only` | Safe-tool classification and policy input. |
| `destructive` | Approval or denial input. |
| `humanApprovalRequired` | Workflow or approval routing input. |
| `sourceProtocol` | Policy and diagnostics dimension. |

Do not rely on model instructions for sensitive operations. If
`destructive: true` or `humanApprovalRequired: true`, enforcement should be in
policy or workflow, not only in the prompt.

See [MCP Tools Access Control](mcp-tools-access-control.md) for invocation
authorization and response filtering.

## Metadata Storage

Use one canonical metadata object and derive indexed columns from it.

Recommended storage:

- Store `toolMetadata` and `inputSchema` as JSON or JSONB in catalog tables.
- Store flattened fields such as `routingDomain`, `semanticNamespace`,
  `sourceProtocol`, `sensitivityTier`, `semanticWeight`, `readOnly`, and
  `destructive` as indexed projection columns when search needs them.
- Regenerate flattened projections when the canonical JSON changes.
- Store `endpointId` as the stable identity for audit, scoring feedback, and
  catalog synchronization.

This avoids drift where `toolMetadata.routing.domain` says one thing and a
flattened `routingDomain` column says another.

The gateway should not read portal catalog tables directly. Light Portal or the
control plane owns catalog authoring, normalization, approval, and projection.
It publishes a flattened runtime config, such as `mcp-router.yml` or
config-cache content, to gateway instances. The gateway then executes from that
approved runtime config and live service discovery state.

Catalog import must normalize compatibility fields before publishing gateway
config. If `safety.read_only` and top-level `read_only` disagree, or
`safety.destructive` and top-level `destructive` disagree, the import should
fail or rewrite the compatibility fields from the canonical `safety` object.
Agents and gateways should never observe conflicting safety values.

## Metadata Contract

The recommended metadata shape is:

```yaml
toolMetadata:
  routing:
    domain: Customers
    semanticNamespace: API0004
    semanticDescription: Get customer preferences
    semanticKeywords:
      - Customers
      - getCustomerPreferences
      - preferences
    semanticWeight: 1.0
    sensitivityTier: internal
    sourceProtocol: openapi
    parameters:
      customerId: path
      channel: query
  safety:
    read_only: true
    idempotent: true
    destructive: false
    humanApprovalRequired: false
  runtime:
    cacheTtlSeconds: 60
    costTier: low
    estimatedLatencyMs: 100
  lifecycle:
    version: 1.0.0
    status: active
  read_only: true
  destructive: false
```

The duplicated top-level `read_only` and `destructive` fields are compatibility
fields. New code should prefer `safety.read_only`, `safety.destructive`, and
`safety.humanApprovalRequired`, then fall back to the top-level fields.

Recommended field semantics:

| Field | Required | Semantics |
|-------|----------|-----------|
| `routing.domain` | Recommended | Business capability group. |
| `routing.semanticNamespace` | Recommended | Catalog/API namespace for filtering and grouping. |
| `routing.semanticDescription` | Recommended | Agent-facing capability summary. |
| `routing.semanticKeywords` | Recommended | Search keywords and aliases. |
| `routing.semanticWeight` | Optional | Ranking multiplier. Default `1.0`. |
| `routing.sensitivityTier` | Recommended | Disclosure and governance tier. |
| `routing.sourceProtocol` | Recommended | Source protocol, such as `mcp`, `openapi`, `http`, or `lightapi`. |
| `routing.parameters` | Required for non-trivial OpenAPI tools | Argument location mapping. |
| `safety.read_only` | Recommended | True when the tool does not mutate state. |
| `safety.idempotent` | Recommended | True when identical calls can be safely retried. |
| `safety.destructive` | Recommended | True when the tool can delete, reset, revoke, overwrite, or cause irreversible effects. |
| `safety.humanApprovalRequired` | Recommended | True when a workflow or approval step must precede execution. |
| `runtime.cacheTtlSeconds` | Optional | Caller-scoped response cache TTL for safe read tools. |
| `runtime.retry` | Optional | Retry policy, only honored for idempotent calls. |
| `runtime.rateLimit` | Optional | Rate-limit grouping and per-call cost units. |
| `runtime.costTier` | Optional | Relative execution cost such as `low`, `medium`, or `high`. |
| `runtime.estimatedLatencyMs` | Optional | Expected latency used for ranking and diagnostics. |
| `lifecycle.version` | Recommended | Tool contract version visible to agents and catalogs. |
| `lifecycle.status` | Recommended | Lifecycle state such as `active`, `deprecated`, or `retired`. |

Use normalized sensitivity reference values such as `public`, `internal`,
`confidential`, and `restricted`. Older imported values such as `Internal-Only`
should be normalized during catalog import or edited through the App/GenAI/Tool
dropdowns.

The current Portal reference tables for this metadata are:

| Reference table | Metadata field | Values |
|-----------------|----------------|--------|
| `sensitivity_tier` | `toolMetadata.routing.sensitivityTier` and `sensitivity_tier` projection | `public`, `internal`, `confidential`, `restricted` |
| `source_protocol` | `toolMetadata.routing.sourceProtocol` and `source_protocol` projection | `openapi`, `mcp`, `lightapi`, `http` |
| `lifecycle_status` | `toolMetadata.lifecycle.status` and `lifecycle_status` projection | `active`, `deprecated`, `retired` |
| `parameter_location` | values inside `toolMetadata.routing.parameters` | `path`, `query`, `header`, `cookie`, `body` |
| `cost_tier` | `toolMetadata.runtime.costTier` and `cost_tier` projection | `low`, `medium`, `high` |

Use `openapi` when the tool contract is generated from an OpenAPI document. Use
`http` for manually configured HTTP-family tools without an OpenAPI contract,
including REST-style endpoints or future HTTP transports such as
GraphQL-over-HTTP and gRPC-over-HTTP.

## Diagnostics

Operators need to understand why an agent saw or did not see a tool and where a
selected call was routed.

Diagnostics should include:

| Event | Useful fields |
|-------|---------------|
| Catalog search | query, selected tool names, scores, score reasons, catalog hash, catalog version. |
| Disclosure filtering | hidden tool names, policy reason, sensitivity tier, destructive flag, approval requirement. |
| Gateway list check | catalog tools missing from gateway, extra gateway tools, gateway list error. |
| Tool call | tool name, endpoint, endpointId, serviceId, envTag, sourceProtocol, policy outcome, correlation ID. |
| Backend routing | target source, selected URL without secrets, discovery node, direct-registry match. |
| Runtime policy | retry attempt, idempotent flag, cache hit or miss, rate-limit decision, cost tier. |
| Response filtering | endpoint, filter rule IDs, filtered result status, policy outcome. |

The agent diagnostics endpoint should compare assigned catalog tools with live
gateway `tools/list` so operators can see catalog/runtime drift.

The gateway should avoid logging tool arguments in full. When arguments are
logged for debugging, masking should follow the `inputSchema` and metadata
sensitivity signals.

Distributed tracing should carry selected metadata as span attributes. Useful
OpenTelemetry attributes include:

| Attribute | Source |
|-----------|--------|
| `mcp.tool.name` | Tool `name`. |
| `mcp.tool.endpoint_id` | `endpointId`. |
| `mcp.tool.endpoint` | `endpoint`. |
| `mcp.tool.domain` | `toolMetadata.routing.domain`. |
| `mcp.tool.namespace` | `toolMetadata.routing.semanticNamespace`. |
| `mcp.tool.source_protocol` | `toolMetadata.routing.sourceProtocol`. |
| `mcp.tool.read_only` | `toolMetadata.safety.read_only`. |
| `mcp.tool.idempotent` | `toolMetadata.safety.idempotent`. |
| `mcp.tool.cost_tier` | `toolMetadata.runtime.costTier`. |

These attributes let operators group latency, errors, policy denials, and rate
limits by domain, namespace, protocol, and tool contract instead of only by URL.

## Advanced Metadata Usage

The same metadata contract can support features beyond search and routing.

### Dry Run And Mocking

Development and workflow validation can use sandbox metadata:

```yaml
toolMetadata:
  sandbox:
    enabled: true
    mode: mock
    mockResponse:
      customerId: CUST-1001
      preferences:
        channel: portal
```

Mocking must be opt-in and environment-scoped. Production gateways should not
return mock responses unless a deployment explicitly enables sandbox mode for a
tool, environment, or test principal.

### UI Rendering Hints

Some tool results are easier to inspect as structured UI components:

```yaml
toolMetadata:
  ui:
    component: customer-profile-card
    resultShape: customerProfile
```

UI metadata should be treated as a rendering hint, not as trusted executable
frontend code. The frontend should map known component names to local UI
components and ignore unknown values.

### Related Tools

Catalog search can use related-tool hints to pre-warm or prioritize likely next
schemas:

```yaml
toolMetadata:
  relatedTools:
    - demo_customer_profile_api_get_customer_preferences
    - demo_offer_decision_api_search_offers
```

Related tools should not bypass assignment, visibility, or `tools/list`
intersection. They only affect ranking and prefetch.

### Sub-Agent Orchestration

In a multi-agent deployment, a tool may require skills owned by a worker agent:

```yaml
toolMetadata:
  orchestration:
    requiredSkills:
      - data_analysis
      - python_execution
    preferredAgentId: analytics-worker
```

The supervisor can use these hints to delegate the user task or to avoid
disclosing a tool to an agent that cannot safely execute the surrounding work.

This is orchestration metadata, not a source protocol. Do not use
`sourceProtocol: agent`. Keep `sourceProtocol` for concrete protocol or
contract sources such as `mcp`, `openapi`, `http`, and `lightapi`.

Do not add orchestration reference tables in the first rollout. If sub-agent
delegation becomes a product feature, reuse the Light Portal agent and skill
model:

- Agent capabilities are the skills assigned to each agent.
- `requiredSkills` should be selected from the existing skill catalog.
- `preferredAgentId`, if present, should refer to an agent in the managed agent
  registry.
- The catalog or supervisor should validate that the preferred agent has the
  required skills.

## Evaluation Feedback

Metadata should also support closed-loop improvement.

Track these signals by `endpointId` and tool `name`:

- Search query text or normalized intent.
- Tool rank and selected rank position.
- Whether the model called the tool.
- Whether the call succeeded.
- Whether the user accepted the result.
- Whether policy denied the call.
- Whether retries, cache hits, rate limits, or cost budgets affected the call.
- Whether schema validation or required arguments failed.

This feedback can tune `semanticKeywords`, `semanticDescription`, and
`semanticWeight` without changing the backend API contract.

## Recommended Rollout

Implement metadata usage incrementally:

1. Normalize imported `inputSchema` and `toolMetadata` to structured JSON.
2. Validate administrative routing fields such as `targetHost` and normalize
   compatibility safety fields.
3. Project searchable fields into catalog columns or search documents.
4. Add keyword search over `name`, `endpointName`, `description`,
   `semanticDescription`, `semanticKeywords`, domain, namespace, and schema
   property names.
5. Apply disclosure filters for assignment, environment, lifecycle, sensitivity,
   destructive tools, and approval-required tools.
6. Intersect selected catalog tools with live gateway `tools/list`.
7. Add diagnostics for selected, hidden, missing, and extra tools.
8. Add retry, cache, rate-limit, and OpenTelemetry attributes after the core
   disclosure path is stable.
9. Add semantic or hybrid search after keyword behavior is proven.
10. Feed evaluation results back into keywords and semantic weights.

Do not start by changing gateway `tools/call`. The gateway execution path is
already the right boundary. The first improvement should be better catalog
search and per-turn tool disclosure.

Semantic vector search is needed for large catalogs, but it should be optional.
Keep keyword plus structured filtering as the baseline implementation, then add
hybrid search as an enhancement for deployments that have enough tools to
justify the extra index and operations cost.

## Example End-To-End Flow

User prompt:

```text
Show customer CUST-1001 preferences and find available travel offers.
```

Catalog search:

1. Matches `customer`, `preferences`, and `CUST-1001` against the customer
   profile tool metadata and schema.
2. Matches `travel` and `offers` against the offer search tool metadata and
   schema.
3. Filters out destructive or approval-required tools.
4. Intersects the selected names with gateway `tools/list`.

Model tool disclosure:

```text
demo_customer_profile_api_get_customer_preferences
demo_offer_decision_api_search_offers
```

Execution:

1. The model calls `demo_customer_profile_api_get_customer_preferences`.
2. The gateway maps `customerId` to the path and `channel` to the query string.
3. The gateway runs `req-acc`.
4. The gateway invokes the downstream customer profile API.
5. The gateway runs `res-fil` if configured.
6. The model receives the MCP result.
7. The model calls `demo_offer_decision_api_search_offers` if more data is
   needed.

The model never receives backend URLs, discovery nodes, or direct-registry
details. It only receives the selected tool schemas.

## Resolved Guidance

The recommended default decisions are:

| Topic | Decision |
|-------|----------|
| Semantic search | Support optional semantic or hybrid vector search. Keyword plus structured filters remain the required baseline. |
| Sensitivity tier | Set a default when API details create endpoints. Store allowed values in the `sensitivity_tier` reference table and expose normalized dropdowns in the App/GenAI/Tool pages. |
| Destructive tools | Require workflow-backed or approval-backed execution for destructive tools. Do not expose them as direct model-callable tools unless approval is configured. |
| List visibility | Apply both portal catalog disclosure policy and gateway `toolsListAccessControl`. The visible set is their intersection with live `tools/list`. |
| Semantic weight | Populate `semanticWeight` when the endpoint is created, then allow authorized updates from the App/GenAI/Tool page. |
| Caching | Use gateway caching first for `tools/list` visibility. Keep `tools/call` response caching backend-owned by default, with gateway response caching only as explicit opt-in metadata. |
| Path parameters | Fail closed when a path-template parameter mapping is missing. OpenAPI import should generate `toolMetadata.routing.parameters`; the gateway should not guess by default. |

If a future deployment needs path-template inference, add an explicit opt-in
field such as `routing.parameterInference: pathTemplate`. The default should
remain explicit mapping because it is safer, easier to audit, and consistent
with OpenAPI parameter locations.
