# Endpoint Identity

## Status

- **Decision state:** Accepted for implementation
- **Owner:** Light Gateway maintainers
- **Decision date:** 2026-08-06
- **Revised:** 2026-08-06
- **Tracking issue:** [networknt/light-fabric#297](https://github.com/networknt/light-fabric/issues/297)

## Purpose

Normal HTTP requests need the method in their endpoint identity. A path alone
cannot distinguish operations such as `GET /v1/models` and
`POST /v1/models`.

The generated access-control snapshot already uses method-qualified keys:

```text
/v1/models@get
/v1/chat/completions@post
```

The gateway previously sent only `/v1/models` to access control, so the
generated `/v1/models@get` rule could not match. This design fixes that
mismatch. It does not introduce schema versions, capability negotiation,
legacy modes, dual rule formats, or changes to generated rules.

## Terms

| Value | Example | Used for |
|-------|---------|----------|
| Request path | `/v1/accounts/123` | Routing, URI rewriting, rate limiting, and path-prefix checks |
| Path template | `/v1/accounts/{accountId}` | Stable endpoint and metrics dimensions |
| HTTP method | `GET` | Transport behavior and endpoint qualification |
| HTTP endpoint | `/v1/accounts/{accountId}@get` | Access control, response filtering, logs, audit, and endpoint metrics |

Paths and endpoints are different values. Code that routes or rewrites a URI
uses a path. Code that identifies an operation uses an endpoint.

## Identity Rules

### Normal HTTP

A normal HTTP endpoint is:

```text
{matched-path-template-or-request-path}@{lowercase-method}
```

Examples:

```text
/v1/models@get
/v1/chat/completions@post
/v1/accounts/{accountId}@patch
```

The matched handler template is preferred because it avoids concrete IDs in
policies, logs, and metric dimensions. The query string is never part of the
endpoint.

HTTP methods are distinct. A GET rule must not authorize POST, PUT, PATCH,
DELETE, HEAD, or OPTIONS on the same path.

### Portal Hybrid Requests

Portal query and command requests multiplex operations over shared transport
paths. Their access-control identity remains the generated logical operation
ID already derived from the request envelope:

```text
lightapi.net/service/getApi/0.1.0
```

The Portal server accepts GET and POST transports with the same semantics, so
the transport method is not added to this logical ID. Existing Portal rules
remain unchanged and match exactly.

### MCP and OpenAPI Tools

Existing tool endpoint rules remain unchanged:

- Native MCP operations use their configured `@call` identity, such as
  `weather@call`.
- OpenAPI-backed tools use the proxied HTTP identity, such as `/offers@get`.

This change does not make MCP catalog fields mandatory and does not couple
access-control rule loading to catalog loading.

### WebSocket

WebSocket connection authorization uses `path@connect`, including the existing
controller endpoint:

```text
/ctrl/mcp@connect
```

The controller identity is anchored to the concrete `/ctrl/mcp` request path,
not to a matched handler template. This preserves the controller route's
fail-closed behavior even when handler configuration uses a template or
wildcard that also matches the controller path.

WebSocket routing still uses the upgrade path. It must not use an HTTP
`@get` endpoint as its connection-policy identity.

## Access-Control Matching

Access control compares the operation as well as the selector:

1. Exact endpoint match.
2. Template or parent-path match only when both identities have the same
   operation suffix.

For example:

| Rule | Request endpoint | Match |
|------|------------------|-------|
| `/v1/models@get` | `/v1/models@get` | Yes |
| `/v1/models@get` | `/v1/models@post` | No |
| `/v1/accounts/{id}@get` | `/v1/accounts/123@get` | Yes |
| `/v1/accounts/{id}@get` | `/v1/accounts/123@delete` | No |

Methodless logical IDs, such as Portal operation IDs, match exactly. They are
not implicitly converted to `@call`, and a qualified HTTP lookup never falls
back to a methodless path rule.

Endpoint parsing splits on the final `@`, allowing selectors such as
`/users/foo@bar.com@get`.

`defaultDeny` keeps its existing meaning after lookup:

- `true`: an unmatched endpoint is denied;
- `false`: an unmatched endpoint is allowed.

The fix is to make the generated rule and runtime endpoint agree, not to alter
that policy setting.

## Consumer Boundaries

| Consumer | Input |
|----------|-------|
| Access control | Endpoint identity |
| Request and response filtering | Endpoint identity |
| Endpoint metrics | Endpoint identity plus existing method field |
| Logs and audit | Endpoint identity |
| Router selection and rewrites | Request path |
| Upstream URI construction | Request path and query |
| Rate limiting | Request path |
| `skipPathPrefixes` | Request path |

Routing code must never append `@method` to an upstream URI. The current router
matches query-rewrite rules against the request path first and accepts endpoint
only as a secondary lookup key; it constructs the upstream URI exclusively
from the original and rewritten path. That existing fallback does not append
the endpoint to the path and does not need to change for this issue.

## Metrics

The endpoint dimension includes the operation:

```text
endpoint=/v1/accounts/{accountId}@get
method=GET
pathTemplate=/v1/accounts/{accountId}
```

The separate method field remains useful for method-wide aggregation.
`pathTemplate` provides path-oriented aggregation without parsing the endpoint.
When no template matches, use the bounded `<unmatched>` value rather than the
concrete request path.

Adding `pathTemplate` is an observability change only. It does not change rule
or routing configuration.

## Request Flow

```text
HTTP request
  -> preserve request path and method
  -> resolve handler and matched path template
  -> render path-template@lowercase-method
  -> authorize and filter with that endpoint
  -> record endpoint metrics
  -> route and build the upstream URI from path values
```

Portal, MCP, and WebSocket handlers replace or select the access-control
identity at their existing protocol boundary as described above.

## Development Cutover

There is one endpoint contract, with no transition mode:

- normal HTTP uses `path@method`;
- Portal logical IDs remain methodless;
- MCP uses existing `@call` identities;
- WebSocket uses `@connect`.

The current generated snapshot already follows this contract. No rule or
configuration changes are required. Deploy the gateway code and restart the
development environment together. If a development snapshot contains a
methodless normal HTTP key, regenerate it instead of adding a runtime fallback.

## Required Tests

| Scenario | Expected result |
|----------|-----------------|
| `GET /v1/models` with `/v1/models@get` rule | Allowed when its rule permits access |
| `POST /v1/models` with only a GET rule | Does not match the GET rule |
| Template route `/v1/accounts/{id}` | Uses `/v1/accounts/{id}@method` |
| Response filtering | Uses the same endpoint as authorization |
| Portal GET and POST transports | Resolve to the same generated logical ID |
| Native MCP tool | Keeps its configured `@call` identity |
| OpenAPI-backed tool | Keeps its proxied HTTP identity |
| WebSocket upgrade | Uses `path@connect` for policy |
| Router and upstream URI | Never receive an `@operation` suffix |
| Metrics | Emit endpoint, method, and stable path template |

Tests cover both `defaultDeny` values so a method mismatch cannot be mistaken
for a successful rule match.

## Decisions

- Only normal HTTP endpoint construction changes for issue #297.
- Existing generated rules and configuration are not changed.
- No endpoint schema version or gateway capability is introduced.
- No legacy identity mode or dual lookup is implemented.
- HTTP endpoint methods are lowercase.
- Access control and response filtering match the exact operation.
- Portal, MCP, and WebSocket retain their existing protocol identities.
- Routing, URI rewriting, rate limiting, and path-prefix behavior remain
  path-based.
