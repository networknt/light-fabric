# Light Rule In Light-Gateway

`light-gateway` uses Light-Rule to enforce deterministic policy decisions in
the Pingora request path. Rules are written as inline
[CEL](https://cel.dev/) expressions and evaluated entirely within the gateway
process — no external policy service is required.

The first production use is MCP tool authorization (`req-acc`) and response
filtering (`res-fil`) for the `mcp` handler.

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
`light-rule`. The rule engine is part of the gateway binary; there is no
dynamic plugin loading step.

## Request Flow

For MCP traffic, the runtime flow is:

```text
POST /mcp
  -> handler.yml selects mcp
  -> mcp-router parses JSON-RPC tools/call
  -> access-control runtime builds rule context
  -> light-rule evaluates req-acc CEL expressions
  -> denied: return JSON-RPC error -32001
  -> allowed: call downstream HTTP or MCP tool
  -> light-rule evaluates optional res-fil CEL expressions
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

The `security` handler must run before `mcp` so that JWT claims are decoded
and available in the rule context when CEL expressions are evaluated.

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
    description: Get current weather for a city.
    targetHost: http://weather-api:8080
    path: /weather
    method: GET
    endpoint: weather@call
    apiType: http
    inputSchema:
      type: object
      properties:
        city:
          type: string
      required:
        - city
```

The `endpoint` field is the stable policy key used in `rule.yml`. If it is
omitted, the gateway derives one from the tool name and method, such as
`weather@call`.

`maxSessions` caps the total in-memory MCP frontend sessions for this gateway
process. `maxSessionsPerClient` caps sessions for one authenticated client or,
when no principal is available, one MCP `clientInfo.name` and
`clientInfo.version` pair.

For downstream MCP servers, set `apiType: mcp`. For downstream REST API
servers, use `apiType: http` or omit it when the default is acceptable.

### access-control.yml

`access-control.yml` controls whether policy is active and how rules combine:

```yaml
enabled: true
accessRuleLogic: any
defaultDeny: true
defaultInclude: false
skipPathPrefixes: []
logFullCelContext: false
```

Fields:

- `enabled`: turns access-control evaluation on or off.
- `accessRuleLogic`: `any` (allow if any rule passes) or `all` (allow only if
  every rule passes) for `req-acc` rule IDs on an endpoint.
- `defaultDeny`: when `true`, deny calls with no matching endpoint rule.
- `defaultInclude`: when `false`, a response row filter with no matching caller
  role, group, position, attribute, or user entry returns no rows. Set `true`
  only to preserve the legacy include-all row-filter behavior.
- `skipPathPrefixes`: endpoint prefixes that bypass access control entirely.
- `logFullCelContext`: controls CEL context values in `light_rule::cel` trace
  events. The default `false` reports only statically referenced paths and
  structural metadata. Set it to `true` only for local or development debugging
  to include the bounded values of statically referenced properties. This
  property does not enable trace logging; use a filter such as
  `RUST_LOG=light_rule::cel=trace,info`.

The file name is `access-control.yml`. The loader also accepts
`access-control.yaml`.

### rule.yml

`rule.yml` holds the CEL rule bodies and maps them to endpoints:

```yaml
ruleBodies:
  allow-scp-group.lightapi.net:
    ruleId: allow-scp-group.lightapi.net
    ruleName: Allow request when scp claim contains the required group
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'scp' in auditInfo.subject_claims.ClaimsMap
      && 'groups' in permission
      && permission.groups in auditInfo.subject_claims.ClaimsMap.scp

endpointRules:
  weather@call:
    req-acc:
      - allow-scp-group.lightapi.net
    permission:
      groups: weather.r
```

**Key fields in each rule body:**

| Field | Required | Description |
|-------|----------|-------------|
| `ruleId` | yes | Unique identifier, referenced from `endpointRules`. |
| `ruleName` | yes | Human-readable description. |
| `ruleType` | yes | `req-acc` for request authorization, `res-fil` for response filtering. |
| `conditionLanguage` | yes | Must be `cel`. |
| `conditionSecurityProfile` | yes | Must be `strict` (see [Security Profile](#security-profile)). |
| `expression` | yes | CEL expression that must return `true` to allow the request. |
| `version` | yes | Semantic version string. |
| `common` | no | `"Y"` marks the rule as shared across hosts. |
| `actions` | yes | Must be an empty list `[]` — action-based dispatch is not supported. |

**Key fields in each endpoint rule entry:**

| Field | Description |
|-------|-------------|
| `req-acc` | List of rule IDs evaluated before calling the downstream tool. |
| `res-fil` | List of rule IDs evaluated after the downstream response. |
| `permission` | Arbitrary key/value map injected into the CEL context as `permission`. Keeps rule bodies generic and reusable. |

The file name is `rule.yml`. The loader also accepts `rule.yaml`.

## Rule Context

For every MCP tool call the gateway builds a CEL evaluation context containing
the following top-level variables:

| Variable | Type | Description |
|----------|------|-------------|
| `auditInfo` | map | Decoded JWT claims and correlation metadata. |
| `permission` | map | The per-endpoint `permission` object from `endpointRules`. |
| `headers` | map | Normalised (lowercased) HTTP request headers. |
| `toolName` | string | MCP tool name from the `tools/call` request. |
| `toolArguments` | map | Tool call arguments from the `tools/call` request. |
| `endpoint` | string | Endpoint identifier, e.g. `weather@call`. |
| `correlationId` | string | Correlation ID when one is present. |

JWT claims are nested under `auditInfo.subject_claims.ClaimsMap`. The gateway
normalises common fields automatically:

| CEL path | JWT source | Type |
|----------|------------|------|
| `auditInfo.subject_claims.ClaimsMap.scp` | `scp` | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.roles` | `roles` | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.positions` | `positions` | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.groups` | `groups` | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.attributes` | `attributes` | `map<string,string>` |
| `auditInfo.subject_claims.ClaimsMap.sub` | `sub` | `string` |
| `auditInfo.subject_claims.ClaimsMap.client_id` | `client_id` / `azp` | `string` |
| `auditInfo.subject_claims.ClaimsMap.uid` | user ID injected by gateway | `string` |
| `auditInfo.subject_claims.ClaimsMap.role` | `role` (singular) | `string` |

For a full reference with worked examples for every claim type, see
[Request Access Control Rules](request-access-rule.md).

## Security Profile

Rules must declare `conditionSecurityProfile: strict`. The strict profile:

- Restricts available functions and macros to a safe, well-known subset.
- Prevents access to undeclared variables, guarding against injection.
- Causes the expression to return an error (treated as `denied`) if it
  references a missing variable rather than silently returning `false`.

Always guard list membership with an `in` check before accessing a key.
Claims absent from the token will be missing from `ClaimsMap`, and an
unguarded access will be denied:

```cel
# WRONG — will error if 'scp' is not in the token
permission.groups in auditInfo.subject_claims.ClaimsMap.scp

# CORRECT
'scp' in auditInfo.subject_claims.ClaimsMap
&& permission.groups in auditInfo.subject_claims.ClaimsMap.scp
```

## Common CEL Patterns

### Scope (`scp`) — OAuth 2.0 access token

```yaml
expression: |
  'scp' in auditInfo.subject_claims.ClaimsMap
  && 'groups' in permission
  && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
```

### Role

```yaml
expression: |
  'roles' in auditInfo.subject_claims.ClaimsMap
  && 'role' in permission
  && permission.role in auditInfo.subject_claims.ClaimsMap.roles
```

### Position

```yaml
expression: |
  'positions' in auditInfo.subject_claims.ClaimsMap
  && 'position' in permission
  && permission.position in auditInfo.subject_claims.ClaimsMap.positions
```

### Attribute

```yaml
expression: |
  'attributes' in auditInfo.subject_claims.ClaimsMap
  && 'attributeKey' in permission
  && 'attributeValue' in permission
  && permission.attributeKey in auditInfo.subject_claims.ClaimsMap.attributes
  && auditInfo.subject_claims.ClaimsMap.attributes[permission.attributeKey] == permission.attributeValue
```

### AND — require both a scope group and a role

```yaml
expression: |
  'scp' in auditInfo.subject_claims.ClaimsMap
  && 'roles' in auditInfo.subject_claims.ClaimsMap
  && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
  && permission.role in auditInfo.subject_claims.ClaimsMap.roles
```

For OR logic and a fully generic multi-claim rule, see
[Request Access Control Rules](request-access-rule.md).

## Endpoint Matching

When the gateway looks up the rule list for an incoming request it checks:

1. Exact endpoint key — e.g. `weather@call`.
2. Path templates — e.g. `accounts/{id}@get`.
3. Parent path — e.g. `accounts@get` matches `accounts/123@get`.

For MCP tools, always set `endpoint` explicitly in `mcp-router.yml` so the
policy key remains stable even if the downstream path changes.

## Reload Behavior

`light-gateway` supports live reload for MCP and access-control config:

- Reloading `mcp-router.yml` rebuilds the MCP router runtime.
- Reloading `access-control.yml` or `rule.yml` rebuilds the MCP and WebSocket
  policy runtimes.

This matches the product model where `light-portal` manages configuration and
config-server delivers the resolved files.

## Operational Notes

- If `access-control.yml` is missing, MCP tools are allowed unless another
  handler blocks the request.
- If `access-control.yml` is enabled and `defaultDeny: true`, a tool call with
  no matching `req-acc` endpoint rule is denied.
- If `access-control.yml` is enabled and `defaultInclude: false`, a `res-fil`
  row filter with no matching caller claim returns no rows rather than all
  rows.
- If the `security` handler does not run before `mcp`, JWT claims are absent
  and CEL expressions that reference `auditInfo` will deny.
- Rule execution is local to the gateway. No database call is made per request.
- `x-mask` and `x-mask-pattern` in MCP tool `inputSchema` are applied before
  the downstream call. `x-tokenize` is reserved for the tokenization service
  integration.

## Verification

Useful checks:

```bash
cargo tree -p light-gateway -i light-rule
cargo test -p light-pingora access_control
cargo test -p light-gateway gateway_loads_mcp_router_when_mcp_handler_is_active
```

The first command verifies the binary linkage. The test commands verify the MCP
access-control path, default deny behavior, CEL-based allow behavior, and
gateway MCP runtime loading.

## See Also

- [Request Access Control Rules](request-access-rule.md) — full reference for
  CEL `req-acc` rules: JWT claim paths, `permission` object structure, worked
  examples for scopes, roles, positions, attributes, subject, client ID,
  combined conditions, and a generic dynamic rule pattern.
