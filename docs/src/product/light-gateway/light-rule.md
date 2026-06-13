# Light Rule In Light-Gateway

`light-gateway` uses Light-Rule to enforce deterministic policy decisions in
the Pingora request path. The first production use is MCP tool authorization and
response filtering for the `mcp` handler.

This lets a gateway route agent MCP traffic to downstream MCP servers or API
servers while enforcing fine-grained authorization locally from configuration
delivered by config-server.

## When It Runs

Light-Rule is invoked by `light-gateway` when all of these are true:

- `handler.yml` includes the `mcp` handler in the matched chain.
- `mcp-router.yml` enables the MCP router and defines tools.
- `access-control.yml` and/or `rule.yml` are available from local config or
  config-server.
- A client sends `tools/call` to the configured MCP endpoint, normally `/mcp`.

The dependency path is:

```text
light-gateway
  -> light-pingora
  -> light-rule
```

`light-gateway` links `light-pingora`, and `light-pingora` links
`light-rule`. The rule engine is therefore part of the gateway binary; there is
no dynamic plugin loading step.

## Request Flow

For MCP traffic, the runtime flow is:

```text
POST /mcp
  -> handler.yml selects mcp
  -> mcp-router parses JSON-RPC tools/call
  -> access-control runtime builds rule context
  -> light-rule evaluates req-acc rules
  -> denied: return JSON-RPC error -32001
  -> allowed: call downstream HTTP or MCP tool
  -> light-rule evaluates optional res-fil rules
  -> return JSON-RPC result
```

Authorization happens before the downstream call. Response filtering happens
after the downstream response and before the MCP JSON-RPC response is returned
to the agent.

## Required Files

### handler.yml

The `mcp` handler must be in the execution chain for the MCP path:

```yaml
handlers:
  - correlation
  - security
  - mcp

paths:
  - path: /mcp
    method: POST
    exec:
      - correlation
      - security
      - mcp

defaultHandlers: []
```

Security should run before `mcp` when rules depend on JWT claims such as
`role`, `grp`, `pos`, `att`, `uid`, or `sub`.

### mcp-router.yml

`mcp-router.yml` exposes the MCP endpoint and maps tools to downstream APIs or
downstream MCP servers:

```yaml
enabled: true
path: /mcp
maxSessions: 10000
maxSessionsPerClient: 100
tools:
  - name: weather
    description: Get weather.
    targetHost: http://weather-api:8080
    path: /weather
    method: GET
    endpoint: /weather@get
    apiType: http
    inputSchema:
      type: object
      properties:
        city:
          type: string
```

The `endpoint` field is the stable policy key used by `rule.yml`. If it is
omitted, the gateway derives one from the tool path and method, such as
`/weather@get`.

`maxSessions` caps the total in-memory MCP frontend sessions for this gateway
process. `maxSessionsPerClient` caps sessions for one authenticated client or,
when no principal is available, one MCP `clientInfo.name` and
`clientInfo.version` pair.

For downstream MCP servers, set `apiType: mcp`. For downstream API servers, use
`apiType: http` or omit it when the default is acceptable.

### access-control.yml

`access-control.yml` controls whether policy is active and how rules combine:

```yaml
enabled: true
accessRuleLogic: any
defaultDeny: true
skipPathPrefixes: []
```

Fields:

- `enabled`: turns access-control evaluation on or off.
- `accessRuleLogic`: `any` or `all` for `req-acc` rule ids on an endpoint.
- `defaultDeny`: when `true`, deny calls with no matching endpoint rule.
- `skipPathPrefixes`: endpoint prefixes that bypass access control.

The file name is `access-control.yml`. The loader also accepts
`access-control.yaml`.

### rule.yml

`rule.yml` provides the rules and endpoint mappings:

```yaml
ruleBodies:
  allowMcpReader:
    common: Y
    ruleId: allowMcpReader
    ruleName: Allow MCP reader
    ruleType: req-acc
    conditions:
      - operatorCode: isNotNull
        propertyPath: auditInfo.subject_claims.ClaimsMap.role
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction

endpointRules:
  /weather@get:
    req-acc:
      - allowMcpReader
    permission:
      roles: mcp-reader
```

In this example, a caller is allowed only when the authenticated principal has a
role matching `mcp-reader`.

The file name is `rule.yml`. The loader also accepts `rule.yaml`.

## Rule Context

For MCP tool calls, the gateway builds a rule context with:

- `auditInfo.subject_claims.ClaimsMap`: normalized JWT claims from the security
  handler.
- `headers`: incoming agent request headers, lowercased.
- `endpoint`: the tool policy endpoint, for example `/weather@get`.
- `toolName`: the MCP tool name.
- `toolArguments`: the JSON arguments from `tools/call`.
- `correlationId`: the correlation id when one is available.
- `permission`: endpoint permission values merged into the root context.

The current built-in access-control action checks the caller role against
`permission.roles`.

Response filter actions can also use these claim dimensions:

```text
role
group or grp
position or pos
attribute or att
user, user_id, uid, or sub
```

## Built-In Actions

The gateway registers Rust actions under Java-compatible class names:

```text
com.networknt.rule.RoleBasedAccessControlAction
RoleBasedAccessControlAction
com.networknt.rule.ResponseColumnFilterAction
ResponseColumnFilterAction
com.networknt.rule.ResponseRowFilterAction
ResponseRowFilterAction
```

### RoleBasedAccessControlAction

Used with `req-acc`. It compares the caller role claim to `permission.roles`.
If there is no role claim or no configured roles, the action returns denied.

### ResponseColumnFilterAction

Used with `res-fil`. It filters fields from array-like JSON responses according
to endpoint permission configuration.

Example:

```yaml
ruleBodies:
  filterColumns:
    common: Y
    ruleId: filterColumns
    ruleName: Filter account columns
    ruleType: res-fil
    conditions:
      - operatorCode: isNotNull
        propertyPath: col
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction

endpointRules:
  /accounts@get:
    res-fil:
      - filterColumns
    permission:
      col:
        role:
          mcp-reader: '["id","name"]'
```

### ResponseRowFilterAction

Used with `res-fil`. It filters rows from array-like JSON responses according to
configured row predicates.

Example:

```yaml
ruleBodies:
  filterRows:
    common: Y
    ruleId: filterRows
    ruleName: Filter account rows
    ruleType: res-fil
    conditions:
      - operatorCode: isNotNull
        propertyPath: row
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction

endpointRules:
  /accounts@get:
    res-fil:
      - filterRows
    permission:
      row:
        role:
          mcp-reader:
            - colName: status
              operator: "="
              colValue: "OPEN"
```

## Matching Rules

Endpoint matching checks:

- exact endpoint key first
- Java-style path templates such as `/accounts/{id}@get`
- parent path entries, for example `/accounts@get` for `/accounts/123@get`

For MCP tools, prefer explicitly setting `endpoint` in `mcp-router.yml` so the
policy key remains stable even if the downstream path changes.

## Reload Behavior

`light-gateway` has reload support for MCP and access-control config:

- reloading `mcp-router.yml` rebuilds the MCP router runtime
- reloading `access-control.yml` or `rule.yml` rebuilds MCP and WebSocket policy
  runtimes

This matches the product model where `light-portal` manages configuration and
config-server delivers the resolved files.

## Operational Notes

- If `access-control.yml` is missing, MCP tools are allowed unless another
  handler blocks the request.
- If `access-control.yml` is enabled and `defaultDeny` is `true`, a tool call
  with no matching `req-acc` endpoint rule is denied.
- If the security handler does not run before `mcp`, role-based rules will not
  have caller claims and will deny.
- Rule execution is local to the gateway. It does not call the database on each
  request.
- `x-mask` and `x-mask-pattern` in MCP tool `inputSchema` are handled before
  downstream execution. `x-tokenize` is reserved for the tokenization service
  integration.

## Verification

Useful checks:

```bash
cargo tree -p light-gateway -i light-rule
cargo test -p light-pingora access_control
cargo test -p light-gateway gateway_loads_mcp_router_when_mcp_handler_is_active
```

The first command verifies the binary linkage. The test commands verify the MCP
access-control path, default deny behavior, role-based allow behavior, response
filtering, and gateway MCP runtime loading.
