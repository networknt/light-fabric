# MCP Router

## Status

Phases 1, 2, 3, and 4 are implemented in `light-pingora` and
`light-gateway`. The configurable tokenization client remains deferred until
`light-tokenization` is migrated to `portal-service/apps/portal-service` and
the protocol is selected. Stateful backend MCP session mapping is implemented
for the single-process gateway session store and documented below.

## Purpose

The Java `mcp-router` module exposes a configured Model Context Protocol
endpoint, `/mcp` by default, and turns configured gateway targets into MCP
tools. AI agents can call `initialize`, `tools/list`, and `tools/call`; the
router then forwards the tool call to an HTTP service or another MCP server.

In light-fabric this should be a `light-pingora` handler that is activated by
`light-gateway` through `handler.yml`. The same gateway binary can contain the
MCP router implementation, but each product decides whether it runs by including
the `mcp` handler and the `mcp-router.yml` configuration from the config server.

This feature is separate from the existing runtime MCP control plane in
`light-runtime`. Runtime MCP is an internal management surface exposed through
the portal registry connection. The MCP router is an HTTP-facing gateway
feature and is subject to the normal inbound handler chain.

The transport target is MCP Streamable HTTP as defined by the current MCP
transport specification:
<https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>.

## Goals

- Keep the Java configuration model recognizable: `enabled`, `path`, and
  `tools`.
- Allow `mcp-router.tools` to be injected by the config server the same way
  `handler.handlers`, `handler.chains`, `handler.paths`, and
  `handler.defaultHandlers` are injected.
- Activate the router with the existing `mcp` handler id in `handler.yml`.
- Expose one MCP endpoint with Streamable HTTP semantics, so `/mcp` is the only
  public MCP path for both POST messages and optional GET streams.
- Support MCP JSON-RPC methods needed by the Java module:
  `initialize`, `notifications/initialized`, `tools/list`, and `tools/call`.
- Route tools to direct `targetHost` endpoints, discovered `serviceId` targets,
  and backend MCP servers.
- Reuse existing cross-cutting handlers such as correlation, security, CORS,
  rate limit, header, metrics, and proxy routing where the chain order allows.
- Register the router configuration with the module registry so it can be
  inspected and reloaded consistently with other light-fabric modules.

## Non-Goals

- Do not use Rust dynamic plugins or `inventory` for runtime tool registration.
  The active tools are product configuration, not compile-time discovery.
- Do not merge the public MCP router and the internal runtime MCP control plane
  into one handler.
- Do not implement a full MCP server framework in the first pass. The gateway
  only needs the methods used by agents to discover and call configured tools.
- Do not copy Java's legacy HTTP+SSE endpoint split as the target transport.
  Streamable HTTP is the Rust target; legacy SSE can be considered only as a
  compatibility mode if an older client requires it.
- Do not hardcode tokenization or masking service URLs. Java currently has a
  hardcoded tokenization endpoint in this path; the Rust port should make that
  configurable when masking/tokenization is added.

## Java Behavior To Map

The Java module has three main pieces:

- `McpConfig` loads `mcp-router.yml` with `enabled`, `path`, and `tools`.
- `McpHandler` owns the HTTP MCP endpoint and JSON-RPC protocol handling.
- `McpToolRegistry` stores configured tool implementations by name.

Java configuration:

```yaml
enabled: ${mcp-router.enabled:true}
path: ${mcp-router.path:/mcp}
maxSessions: ${mcp-router.maxSessions:10000}
maxSessionsPerClient: ${mcp-router.maxSessionsPerClient:100}
tools: ${mcp-router.tools:}
```

Each tool supports these fields:

```yaml
- name: weather
  description: Get weather information
  protocol: http
  serviceId: com.networknt.weather-1.0.0
  envTag: dev
  targetHost: http://localhost:7081
  path: /weather
  method: GET
  endpoint: /weather@get
  apiType: http
  inputSchema:
    type: object
    properties:
      city:
        type: string
  toolMetadata: {}
```

The Java handler currently supports:

- `GET /mcp` as an SSE compatibility endpoint. It creates a session id and
  emits an `endpoint` event pointing to `/mcp?sessionId=...`.
- `POST /mcp` for JSON-RPC messages.
- `initialize`, returning protocol version, tool capabilities, and server info.
- `notifications/initialized`, returning no response.
- `tools/list`, optionally filtered by `params.query` or `params.intent`.
- `tools/call`, forwarding arguments to the configured tool.

The Java tool execution supports two target types:

- HTTP tools call a configured HTTP endpoint. `GET` maps arguments to query
  parameters. Other methods send the arguments as a JSON body.
- MCP proxy tools call a backend MCP server by sending a JSON-RPC
  `tools/call` request to the configured backend path.

Java also includes rule-based access checks, response filtering, masking, and
tokenization around tool calls. The Rust version now implements access checks,
response filtering, and schema-driven request masking without hardcoded service
endpoints. Tokenization is intentionally deferred.

The Rust implementation should map this behavior to MCP Streamable HTTP rather
than keeping Java's legacy HTTP+SSE transport as the default. Streamable HTTP
uses one MCP endpoint path. Clients send JSON-RPC messages with `POST /mcp`;
the server can return either a single `application/json` response or
`text/event-stream` from that same POST when streaming is needed. Clients may
also issue `GET /mcp` to open an optional server-to-client SSE stream on the
same endpoint.

## Resolved Decisions

- Use Streamable HTTP so only one public MCP endpoint, normally `/mcp`, is
  exposed.
- Defer the tokenization client design until `light-tokenization` is migrated
  into `portal-service/apps/portal-service` and its protocol is selected.
- Reuse the light-4j `access-control.yml` compatibility contract for MCP,
  REST, and JSON-RPC authorization.
- Do not add configured per-tool outbound headers. Backend tool calls should
  pass through the headers received from the agent, subject only to headers
  that the HTTP client must regenerate for a new outbound request and MCP
  session headers that the gateway must map or regenerate.

## Rust Architecture

Add the MCP router to `light-pingora` because it is a request/response gateway
handler. `light-gateway` should wire it into the existing handler descriptor
table and runtime state.

Proposed modules:

```text
frameworks/light-pingora/src/access_control.rs
frameworks/light-pingora/src/mcp.rs
```

Primary types:

```rust
pub struct McpRouterConfig {
    pub enabled: bool,
    pub path: String,
    pub tools: Vec<McpToolConfig>,
}

pub struct McpToolConfig {
    pub name: String,
    pub description: String,
    pub protocol: Option<String>,
    pub service_id: Option<String>,
    pub env_tag: Option<String>,
    pub target_host: Option<String>,
    pub path: String,
    pub method: HttpMethod,
    pub endpoint: Option<String>,
    pub api_type: McpToolType,
    pub input_schema: serde_json::Value,
    pub tool_metadata: serde_json::Value,
}

pub struct McpRouterRuntime {
    pub config: ArcSwap<McpRouterConfig>,
    pub client: reqwest::Client,
    pub registry_client: Option<Arc<PortalRegistryClient>>,
}
```

The exact field names should follow the existing light-fabric serde naming
style while accepting the Java config names through aliases:

- `serviceId`
- `envTag`
- `targetHost`
- `apiType`
- `inputSchema`
- `toolMetadata`

`mcp-router.yml` should be the primary Rust file name, but the loader should
also accept `mcp-router.yaml` for Java compatibility.

### Tool Registration

The router does not need global static registration. Build an immutable tool map
when `mcp-router.yml` is loaded:

```text
McpRouterConfig -> BTreeMap<String, McpToolConfig> -> Arc<McpRouterState>
```

On reload, build a new state and atomically swap the `Arc`. In-flight requests
continue with the old state.

This is simpler than Java's static `McpToolRegistry` and avoids Rust plugin
complexity. It also matches the light-fabric product model: all handlers can be
linked into one binary, while the config server decides which handlers and tools
are active for a product.

### Request Flow

The `mcp` handler should participate in the normal handler chain:

```text
request
  -> correlation
  -> metrics
  -> cors
  -> security or unified security
  -> limit
  -> mcp
  -> proxy or route handler, only if mcp did not consume the request
response
  -> header
  -> metrics
  -> access log
```

When the request path matches `mcp-router.path`:

- `POST` parses a JSON-RPC message. Requests return either
  `application/json` for a single response or `text/event-stream` for a
  streamed response on the same endpoint. Notifications and JSON-RPC responses
  sent by the client return `202 Accepted` with no body when accepted.
- `GET` with `Accept: text/event-stream` may open a server-to-client SSE stream
  on the same endpoint. If the gateway has no server-initiated messages to
  stream, it should return `405 Method Not Allowed`.
- `DELETE` should terminate the gateway session and any mapped backend MCP
  sessions. Until session termination is implemented, it can return
  `405 Method Not Allowed`.
- Other methods return `405 Method Not Allowed`.

When the path does not match, the handler continues to the next handler in the
configured chain.

The handler must be safe to include in shared chains. If `mcp-router.enabled` is
false, or the `mcp` handler is not in `handler.yml`, no MCP route is exposed.

### JSON-RPC Handling

Supported methods:

```text
initialize
notifications/initialized
tools/list
tools/call
```

`initialize` response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2024-11-05",
    "capabilities": {
      "tools": {
        "listChanged": true
      }
    },
    "serverInfo": {
      "name": "light-gateway-mcp",
      "version": "1.0.0"
    }
  }
}
```

`tools/list` returns configured tools with `name`, `description`, and
`inputSchema`. It should preserve Java's simple filtering:

- `params.query` matches tool name or description.
- `params.intent` matches tool name or description.

`tools/call` validates `params.name`, finds the tool, validates or forwards
`params.arguments`, and returns either:

```json
{
  "content": [
    {
      "type": "text",
      "text": "..."
    }
  ]
}
```

or the structured result returned by the backend MCP server.

JSON-RPC errors should use the same codes as Java where practical:

```text
-32700 parse error
-32601 method or tool not found
-32602 invalid params
-32000 tool execution failed
-32001 access denied
```

Rust improvement: malformed transport payloads should return a clear HTTP `400`
with a JSON-RPC error body instead of a generic HTTP `500`.

For Streamable HTTP:

- Clients must send each JSON-RPC message as a separate `POST` to the MCP
  endpoint.
- Clients should send `Accept: application/json, text/event-stream`.
- The router should negotiate and honor `MCP-Protocol-Version`.
- The router terminates the client-facing MCP session. `initialize` responses
  should include a gateway-owned `Mcp-Session-Id`, and later client requests
  should be validated against that gateway session.

### MCP Session Management

The MCP router should use a facade model. To the agent, `light-gateway` is the
MCP server. To upstream MCP targets, `light-gateway` is an MCP client. This
keeps gateway security, access-control policy, masking, response filtering, and
tool aggregation in one place while still respecting upstream MCP session
state.

There are two distinct session scopes:

- Frontend session: the session between the MCP client and `light-gateway`.
- Backend session: one upstream MCP server session owned by the gateway for a
  specific frontend session and backend target.

The frontend session is created during client `initialize`:

1. The client sends `initialize` to `mcp-router.path`.
2. The gateway returns the MCP capabilities it exposes and a gateway-generated
   `Mcp-Session-Id`.
3. The gateway stores session state keyed by that id. The state should include
   the negotiated protocol version, client info, security principal or relevant
   auth context, and any backend MCP sessions created for this client session.
4. Later client requests must include the gateway session id. Unknown or expired
   session ids should fail before tool execution.
5. A client `DELETE` request, explicit expiry, or gateway shutdown should close
   all backend sessions associated with the frontend session.

The in-memory gateway store uses a 30-minute idle timeout, a configurable
maximum frontend session count, and a configurable per-client frontend session
count. Expired sessions are purged lazily during later MCP requests, and any
mapped backend MCP sessions are closed during that purge. If the store is still
full after lazy purge, or the client already owns the maximum allowed sessions,
new `initialize` requests fail without issuing another session id.

The per-client key is derived from the authenticated principal when available,
preferring `client_id`, then `user_id`, `email`, and `host`. If no security
principal is available, the key falls back to MCP `clientInfo.name` and
`clientInfo.version` from the `initialize` request.

For a single gateway process, the session store can start in memory. In a
multi-pod deployment, the store should be external, such as Redis, or ingress
must provide sticky routing for all requests that carry the same
`Mcp-Session-Id`.

Backend handling depends on the tool type.

For `apiType: http`, the backend is a normal stateless API:

1. No backend MCP session is created.
2. The gateway translates `tools/call` arguments into a normal HTTP request.
3. `GET` tools serialize arguments into the query string; body-capable methods
   send JSON.
4. The gateway wraps the HTTP response into an MCP `tools/call` result.
5. User-specific auth, tenant, correlation, and trace headers come from the
   frontend session or inbound request and are applied to the outbound HTTP
   call as normal gateway headers.

For `apiType: mcp`, the backend is a stateful MCP server:

1. The gateway lazily initializes the backend session the first time a frontend
   session calls a tool for that backend target. If future dynamic tool
   discovery depends on the backend, this initialization can happen before
   `tools/list` instead.
2. The gateway sends `initialize` to the backend MCP endpoint as an MCP client.
   It should use the client-requested protocol version when supported and pass
   only the capabilities it needs upstream.
3. If the backend returns `Mcp-Session-Id`, the gateway stores it in a mapping
   keyed by the gateway session id and backend target identity.
4. The gateway sends `notifications/initialized` to the backend when the
   backend session is established.
5. For later backend calls, the gateway sends the backend session id to that
   backend. It must not forward the frontend gateway session id as if it were a
   backend session id.
6. The gateway still performs access checks before calling the backend and
   response filtering after the backend response.
7. When the frontend session ends, the gateway should terminate each mapped
   backend MCP session to avoid leaking backend resources.

The backend target identity used in the session map should be stable across
requests. It should include the resolved route information that distinguishes
one backend MCP endpoint from another, such as `targetHost` or `serviceId`,
`envTag`, `protocol`, and tool `path`.

When the router aggregates tools from both MCP servers and normal APIs, the
client still sees one gateway MCP session and one `tools/list` response. The
gateway registry decides how each `tools/call` is executed:

| Feature | MCP server backend | Normal API backend |
| --- | --- | --- |
| Config type | `apiType: mcp` | `apiType: http` or omitted |
| Backend session | Yes, mapped from gateway session to backend target | No |
| Initialization | Gateway initializes backend as an MCP client | No upstream initialization |
| Message handling | JSON-RPC `tools/call` through backend MCP session | Translate JSON-RPC arguments to HTTP |
| Backend session header | Send backend `Mcp-Session-Id` only to that backend | Do not send MCP session state |
| Tear-down | Close backend session on client session end | Nothing backend-specific |

The configured `tools/list` remains the gateway's public contract. A future
dynamic-discovery mode may call backend MCP `tools/list` and merge those tools
with configured HTTP tools, but that must still preserve the gateway's policy
surface and avoid exposing backend tools that are not authorized for the
product.

### HTTP Tool Execution

For `apiType: http` or missing `apiType`:

1. Resolve the target base URL.
2. Build the target URL from base URL plus tool `path`.
3. For `GET`, serialize arguments with `url::form_urlencoded`.
4. For `POST`, `PUT`, and `PATCH`, send arguments as JSON.
5. Pass through the inbound agent headers to the backend tool call so caller
   identity, authorization, correlation, tenant, locale, and tracing context are
   preserved.
6. Let the HTTP client regenerate transport-specific headers for the new
   outbound request, such as `Host`, `Content-Length`, `Transfer-Encoding`, and
   connection management headers.
7. Treat 2xx as success.
8. Parse JSON responses as structured MCP results.
9. Wrap non-JSON responses as MCP text content.
10. Return an empty 2xx response as `{ "result": "success" }`.

Target resolution:

- Prefer `targetHost` for direct calls.
- Otherwise use `serviceId`, `protocol`, and `envTag` through the existing
  portal registry discovery client.
- If neither is available, return a tool execution error.

### MCP Proxy Tool Execution

For `apiType: mcp`:

1. Resolve the target base URL the same way as HTTP tools.
2. Ensure a backend MCP session exists for the current gateway session and
   backend target. If none exists, initialize the backend MCP endpoint and store
   the returned backend `Mcp-Session-Id`.
3. POST to the configured backend `path`.
4. Pass through the inbound agent headers to the backend MCP server, with
   transport-specific headers regenerated for the new outbound request.
   Replace any frontend gateway `Mcp-Session-Id` with the mapped backend
   session id for this backend target.
5. Send a backend JSON-RPC request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "tool-name",
    "arguments": {}
  }
}
```

6. If the backend returns `error`, map it to `-32000`.
7. If the backend returns `result`, return it to the caller.
8. On frontend session termination or expiry, close the backend MCP session.

This preserves the Java `McpProxyTool` behavior while using Rust's typed
JSON-RPC models where possible and adds the MCP session mapping required by
stateful backend MCP servers.

## Configuration Loading

The router should be loaded as a normal light-fabric module:

```text
config-server product values
  -> mcp-router.yml placeholders
  -> light-gateway startup
  -> light-pingora mcp router state
```

Example product values:

```yaml
mcp-router.enabled: true
mcp-router.path: /mcp
mcp-router.tools:
  - name: get_pet
    description: Get a pet by id.
    targetHost: http://petstore:8080
    path: /v1/pets
    method: GET
    inputSchema:
      type: object
      properties:
        id:
          type: string
```

Example `handler.yml` path wiring:

```yaml
handlers:
  - correlation
  - metrics
  - cors
  - jwt
  - mcp
  - proxy

chains:
  default:
    - correlation
    - metrics
    - cors
    - jwt
    - proxy
  mcp:
    - correlation
    - metrics
    - cors
    - jwt
    - mcp

paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
  - path: /mcp
    method: GET
    exec:
      - mcp

defaultHandlers:
  - proxy
```

The exact chain names are product choices. The important point is that `/mcp`
can have a narrow chain while normal API proxy traffic keeps the normal proxy
chain.

## Module Registry

The MCP router should register its configuration with the module registry:

- module name: `mcp-router`
- config files: `mcp-router.yml`, with `mcp-router.yaml` as compatibility
  fallback
- enabled status
- configured path
- tool count
- tool names

The module registry should mask any future secret fields in `toolMetadata`,
headers, or credential configuration.

Reload behavior:

1. Reload `mcp-router.yml`.
2. Validate duplicate tool names, missing paths, unsupported methods, and target
   resolution fields.
3. Build a new immutable router state.
4. Swap the runtime state atomically.
5. Report the updated module registry status.

## Security And Policy

The first layer of protection should be the handler chain. Products can place
JWT, API key, basic auth, unified security, CORS, rate limit, and header
handlers before or after `mcp` as needed.

Because MCP Streamable HTTP is browser-reachable, the `mcp` handler must also
validate the `Origin` header according to the configured CORS or security
policy. Invalid origins should fail before tool execution.

Fine-grained tool authorization should be added after the base router:

- Reuse the existing light-4j `access-control.yml` model as the compatibility
  contract. `access-control.yml` controls `enabled`, `accessRuleLogic`,
  `defaultDeny`, and `skipPathPrefixes`; `rule.yml` provides `ruleBodies` and
  `endpointRules`.
- Make the access policy endpoint stable. Java uses the tool `endpoint` field,
  such as `/weather@get`; when omitted, Rust derives `{path}@{method}`.
- Include correlation id, caller claims, request headers, tool name, endpoint,
  and arguments in the policy input.
- Support default deny when access control is enabled and no `req-acc` rule
  matches.
- Provide built-in Rust actions compatible with the Java class names used by
  current config: `RoleBasedAccessControlAction`,
  `ResponseColumnFilterAction`, and `ResponseRowFilterAction`.

Response filtering should be implemented as a second policy stage:

- Apply policy after backend execution and before JSON-RPC response emission.
- Support both `structuredContent` and single text content responses, matching
  Java's behavior.
- Match endpoint rules exactly first, then Java-style path templates and
  parent path entries such as `/v1/accounts@get` for
  `/v1/accounts/123@get`.

### `req-acc` And `res-fil` Rule Design

The MCP router should treat endpoint rules as two separate policy stages:

- `req-acc`: request access rules. These run before backend tool execution.
  They decide whether the caller can invoke the tool at all.
- `res-fil`: response filter rules. These run after backend tool execution and
  before the JSON-RPC result is sent back to the caller. They can remove rows,
  remove columns, or otherwise reduce the returned payload.

Both stages use Light-Rule with CEL rule conditions. Light-Fabric only supports
CEL conditions for new rule execution. The old native condition-row format is a
legacy Java yaml-rule format and is not the Light-Fabric runtime contract. CEL
keeps the rule predicate explicit while still letting the Java-compatible action
classes perform the stable role, row, and column filter behavior. In this model,
CEL decides whether a rule is eligible to run, and `permission` carries the
endpoint-specific role, row, and column policy values.

The endpoint key must be stable and should not include the query string. For
example, the demo request:

```bash
curl -s "http://127.0.0.1:8086/offers?segment=premium&state=ON&category=travel"
```

maps to the endpoint rule key `/offers@get`. The query parameters are part of
the tool arguments and can be inspected by CEL through `toolArguments`.

For the offer demo, the backend should return enough rows to prove filtering,
for example:

```json
[
  {
    "offerId": "OFFER-TRAVEL-01",
    "title": "Premium travel credit",
    "segment": "premium",
    "state": "ON",
    "category": "travel",
    "priority": 1,
    "active": true
  },
  {
    "offerId": "OFFER-TRAVEL-50",
    "title": "Premium lounge bundle",
    "segment": "premium",
    "state": "ON",
    "category": "travel",
    "priority": 50,
    "active": true
  },
  {
    "offerId": "OFFER-TRAVEL-OLD",
    "title": "Retired companion fare",
    "segment": "premium",
    "state": "ON",
    "category": "travel",
    "priority": 10,
    "active": false
  }
]
```

Two roles are enough to demonstrate the behavior:

- `offer-viewer`: can invoke the offer tool, but can only see rows where
  `priority < 50` and `active == true`. The `active` column must not be
  returned.
- `offer-admin`: can invoke the offer tool and can see every row and every
  column, including inactive offers and all priorities.

Example rule mapping:

```yaml
ruleBodies:
  allowOfferSearch:
    common: Y
    ruleId: allowOfferSearch
    ruleName: Allow offer search
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction

  filterOfferRows:
    common: Y
    ruleId: filterOfferRows
    ruleName: Filter offer rows
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200
      && responseBody != ""
      && auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction

  filterOfferColumns:
    common: Y
    ruleId: filterOfferColumns
    ruleName: Filter offer columns
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200
      && responseBody != ""
      && auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction

endpointRules:
  /offers@get:
    req-acc:
      - allowOfferSearch
    res-fil:
      - filterOfferRows
      - filterOfferColumns
    permission:
      roles: offer-viewer offer-admin
      row:
        role:
          offer-viewer:
            - colName: priority
              operator: "<"
              colValue: 50
            - colName: active
              operator: "="
              colValue: true
      col:
        role:
          offer-viewer: offerId,title,segment,state,category,priority
```

`res-fil` order matters. Row filtering must run before column filtering when a
row predicate depends on a column that may be hidden from the final response.
In the example above, `active` is needed to select rows but is then removed for
`offer-viewer`.

`res-fil` always executes as a sequential pipeline. `accessRuleLogic` only
controls how multiple `req-acc` rules are combined; it does not apply to
response filters. The pipeline should parse the MCP result JSON once, pass the
same mutable JSON value through each `res-fil` action, and serialize it back
into the MCP result once after all filters complete.

The current compatibility actions support the existing permission model:

- `roles` is used by `RoleBasedAccessControlAction`.
- `row.role`, `row.group`, `row.position`, `row.attribute`, and `row.user`
  provide row filters for the matching caller claim.
- Row filters support `=`, `!=`, `<`, `>`, `<=`, `>=`, `in`, and `not in`.
- `col.role`, `col.group`, `col.position`, `col.attribute`, and `col.user`
  provide the returned field list for the matching caller claim. A field list
  prefixed with `!` is a remove list; otherwise it is a keep list.
- Column filtering must apply to top-level JSON objects as well as top-level
  arrays and object payloads containing `items`. Row filtering applies only to
  arrays or `items` arrays.

This should remain the default design for Java and Rust parity. If a later
policy needs arbitrary per-row CEL predicates, add a new explicit filter action
such as `ResponseCelRowFilterAction` instead of changing the meaning of the
Java-compatible `ResponseRowFilterAction`.

CEL must not directly manipulate the MCP result JSON. The rule-level CEL
expression decides whether a `res-fil` rule applies; the action performs the
mutation. This keeps result extraction, `structuredContent` handling,
text-content handling, single-pass JSON parsing, failure behavior, and audit
logging inside tested Rust pipeline and action code.

A CEL-aware row action can be added when the permission row-filter format is not
expressive enough:

```yaml
ruleBodies:
  filterOfferRowsWithCel:
    ruleId: filterOfferRowsWithCel
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200 && responseBody != ""
    actions:
      - actionClassName: com.networknt.rule.ResponseCelRowFilterAction
        actionValues:
          rowExpression: >
            auditInfo.subject_claims.ClaimsMap.role == "offer-admin"
            || (row.priority < 50 && row.active == true)
```

Even in that case, CEL only returns a boolean for each row. The action owns row
iteration and mutation of the parsed response value; the response-filter
pipeline owns final serialization and updating the MCP response.

The row action should not deep-clone the full rule context for every response
row. It should use a child CEL context that shadows `row`, or reuse one mutable
context and update only the `row` binding for each evaluation. If a row-level CEL
evaluation fails because the row is missing a referenced field, the action should
drop that row and continue. Invalid action configuration, such as an expression
that fails to compile, should fail the whole filter action closed.

Masking and tokenization handling:

- Preserve Java schema extensions: `x-mask`, `x-mask-pattern`, and
  `x-tokenize`.
- Parse these extensions from `inputSchema` as `serde_json::Value`.
- Apply schema-driven `x-mask` request masking before backend tool execution.
- Keep `x-tokenize` as a future extension point. Do not call a tokenization
  service until the portal-service tokenization protocol is finalized.
- Do not hardcode a tokenization service URL. The tokenization client should be
  designed after `light-tokenization` is migrated into
  `portal-service/apps/portal-service`, whether the final protocol is JSON-RPC,
  MCP, or gRPC.

Per-tool outbound headers would mean headers that the MCP router adds from tool
configuration when it calls a specific backend target, for example a configured
`Authorization`, `X-API-Key`, tenant routing header, or vendor-specific version
header. We do not need that feature. The required behavior is header
pass-through: backend tool calls receive the headers that came from the agent,
while the HTTP client regenerates only the transport-specific headers required
for a valid outbound request. MCP session headers are not normal pass-through
headers. The gateway owns the frontend `Mcp-Session-Id` and maps it to backend
session ids when an upstream MCP server is involved.

## Relationship To Existing Runtime MCP

`light-runtime` already has `RuntimeMcpHandler` for runtime management tools.
That should remain internal and registry-facing.

The gateway MCP router should not automatically expose runtime management
tools. If a product needs that bridge later, add an explicit configured tool
type, for example:

```yaml
apiType: runtime
```

That keeps public agent-facing tools separate from management tools and avoids
accidentally exposing cache, module, or service operations through a public
gateway route.

## Phased Implementation

### Phase 1: Core Router

- Add `mcp-router.yml` config parsing in `light-pingora`.
- Accept `tools` as either a YAML array or a JSON string to match Java config
  server injection behavior.
- Add immutable tool map validation.
- Implement the base Streamable HTTP single endpoint: unary `POST /mcp`,
  `Accept` validation for `application/json` and `text/event-stream`,
  `202 Accepted` for accepted notifications, and `405` for unsupported methods.
- Implement JSON-RPC `initialize`, `notifications/initialized`, `tools/list`,
  and `tools/call`.
- Implement direct `targetHost` HTTP tools.
- Pass through agent request headers to direct HTTP and backend MCP tool calls,
  except MCP session headers that the gateway must map separately.
- Wire the existing `mcp` handler id in `light-gateway`.
- Register module status and config with the module registry.
- Add parser and handler tests.

Status: implemented.

### Phase 2: Discovery And MCP Proxy

- Resolve `serviceId`, `protocol`, and `envTag` through the existing portal
  registry discovery client.
- Implement `apiType: mcp` backend proxy tools.
- Add reload support with atomic state swap.
- Add tests with fake discovery and backend MCP responses.

Status: implemented.

### Phase 3: Streamable HTTP Streaming

- Add streamed `text/event-stream` responses from `POST /mcp` for long-running
  tool calls or server-to-client messages related to the originating request.
- Add optional `GET /mcp` server-to-client streams on the same endpoint.
- Track frontend sessions when `Mcp-Session-Id` is issued. Return `405` for
  standalone GET streams until server-initiated messages are implemented.
- Add tests for content negotiation, `202 Accepted` notifications, streamed
  POST responses, and optional GET behavior.

Status: implemented.

### Phase 4: Policy, Filtering, Masking

- Add tool-level authorization using the `access-control.yml` compatibility
  contract.
- Add response filtering for structured and text MCP results.
- Add schema-driven request masking.
- Add MCP tool-call log fields for tool name, endpoint, duration, status, and
  policy outcome.

Status: implemented for access control, response filtering, and request
masking. Tokenization is deferred until the portal-service tokenization client
is designed.

### Phase 5: Stateful MCP Backend Sessions

- Add a gateway session store keyed by frontend `Mcp-Session-Id`.
- Validate later client requests against the gateway session.
- For `apiType: mcp`, maintain backend session mappings keyed by gateway
  session id and backend target identity.
- Lazily initialize backend MCP sessions by sending backend `initialize`,
  capturing backend `Mcp-Session-Id`, and sending
  `notifications/initialized`.
- Replace the frontend session id with the mapped backend session id on
  upstream MCP calls.
- Terminate mapped backend MCP sessions when the frontend session is deleted,
  expires, or the gateway shuts down.
- Add tests for frontend session validation, backend session creation, backend
  session reuse, and backend session termination.

Status: implemented for the in-memory frontend session store, configurable
global and per-client session caps, 30-minute lazy idle expiry, lazy backend
initialization, backend `Mcp-Session-Id` mapping, backend session reuse, and
explicit `DELETE` teardown. Shutdown cleanup, external session storage, and
multi-backend isolation tests remain future hardening for multi-pod
deployments.

## Testing Strategy

- Config tests:
  - empty config
  - disabled config
  - duplicate tool names
  - `tools` as YAML array
  - `tools` as JSON string
  - `inputSchema` as object and string
- JSON-RPC tests:
  - `initialize`
  - `notifications/initialized`
  - notification returns `202 Accepted`
  - `tools/list`
  - `tools/list` with `query` and `intent`
  - missing method
  - invalid params
  - malformed JSON
- Streamable HTTP tests:
  - single `/mcp` endpoint handles POST
  - POST validates `Accept`
  - unsupported methods return `405`
  - optional GET stream returns `405` until enabled
- Tool execution tests:
  - direct `GET` with encoded arguments
  - direct `POST` with JSON arguments
  - non-JSON backend response
  - empty 2xx backend response
  - non-2xx backend response
  - agent headers are forwarded to backend tool calls
  - discovered service target
  - backend MCP proxy success and error
- Handler chain tests:
  - `/mcp` consumed by `mcp`
  - non-MCP path continues to the next handler
  - disabled router does not expose `/mcp`
- Reload tests:
  - tool added
  - tool removed
  - invalid reload keeps the prior good state

## Remaining Decisions

- Confirm whether Phase 1 includes only unary Streamable HTTP POST or also
  streamed POST responses.
- Decide the tokenization client protocol after `light-tokenization` is
  migrated into `portal-service/apps/portal-service`.
- Map the Java `access-control.yml` schema to Rust policy execution and define
  how it will be shared by REST, JSON-RPC, and MCP handlers.
