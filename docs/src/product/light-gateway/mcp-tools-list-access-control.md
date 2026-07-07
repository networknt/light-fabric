# MCP Tools List Access Control

This document describes a design for filtering MCP `tools/list` results by the
same access-control policy model that protects MCP `tools/call`.

The goal is to avoid showing an agent tools that the current user cannot call,
while keeping `tools/call` authorization as the final enforcement point.

## Background

The MCP router exposes tools from multiple backends through one gateway MCP
endpoint. A tool can represent a downstream MCP operation:

```json
{
  "name": "local_mcp_echo",
  "apiType": "mcp",
  "endpoint": "echo@call",
  "serviceId": "com.networknt.local.mcp-1.0.0"
}
```

or a downstream OpenAPI endpoint:

```json
{
  "name": "demo_offer_decision_api_search_offers",
  "apiType": "openapi",
  "endpoint": "/offers@get",
  "serviceId": "com.networknt.offer.decision-1.0.0"
}
```

The access-control policy uses the tool endpoint key to enforce `req-acc` when
the tool is called:

```json
{
  "echo@call": {
    "req-acc": ["allow-role-based-access-control.lightapi.net"],
    "permission": {
      "roles": "account-manager",
      "groups": "portal.w"
    }
  },
  "/offers@get": {
    "req-acc": ["allow-scp-claim-group-access-control.lightapi.net"],
    "res-fil": [
      "res-column-filter-jwt-claims.lightapi.net",
      "res-row-filter-jwt-claims.lightapi.net"
    ],
    "permission": {
      "roles": "account-manager teller",
      "groups": "portal.w"
    }
  }
}
```

With the current runtime behavior, `tools/list` returns configured tools and
`tools/call` enforces access-control. This is secure for invocation, but it can
expose unusable tools to an agent.

## Problem

The direct way to filter `tools/list` is to run each tool's `req-acc` rules
before returning the list. That is simple but has drawbacks:

- `tools/list` may run many rule evaluations for every agent discovery request.
- Some `req-acc` rules can depend on `toolArguments`, but `tools/list` has no
  call arguments.
- Evaluating a call-time rule with empty arguments can hide tools that would be
  callable with valid arguments, or show tools that later fail for a specific
  argument value.
- `tools/call` must still run authorization, so list filtering cannot replace
  invocation enforcement.

For these reasons, `tools/list` filtering should be treated as a visibility
optimization, not the authoritative authorization decision.

## Design

Add an optional MCP tools-list visibility filter that uses access-control
configuration to decide which tools are visible to the current principal.

The configuration belongs in `access-control.yml` because it controls how the
access-control runtime affects MCP discovery. The MCP router reads the runtime
decision, but the policy switch should live with the rest of the
access-control settings:

```yaml
enabled: true
accessRuleLogic: any
defaultDeny: true
defaultInclude: false
skipPathPrefixes: []
toolsListAccessControl:
  mode: permission
  unknownRuleFallback: hidden
  maxCelEvaluations: 100
  maxCacheEntries: 2000
  claimMappings: {}
```

The filter should support three modes:

| Mode | Behavior |
|------|----------|
| `none` | Current behavior. `tools/list` returns all configured tools after query filtering. |
| `permission` | Recommended default for protected gateways. Use endpoint rules, permission metadata, and JWT claims to cheaply decide tool visibility. |
| `cel` | Optional strict mode. Evaluate configured `req-acc` rules for each listed tool with empty `toolArguments`. This is best-effort and must be documented as argument-insensitive. |

`tools/call` still evaluates `req-acc` in all modes except when access-control
is globally disabled or skipped by `skipPathPrefixes`.

## Permission Mode

`permission` mode should use the same endpoint key that `tools/call` uses:

1. Get the tool endpoint key from `tool.endpoint`, such as `echo@call` or
   `/offers@get`.
2. Apply the access-control global gates.
3. Look up `rule.endpointRules[endpoint]`.
4. Evaluate the endpoint permission metadata against the authenticated
   principal claims.
5. Return only visible tools.

This mode does not execute arbitrary CEL. It recognizes the common permission
shape already used by the MCP router policies:

```json
{
  "permission": {
    "roles": "account-manager teller",
    "groups": "portal.w"
  }
}
```

An endpoint can also provide a dedicated list visibility block. This is the
preferred shape when call-time `req-acc` is complex or argument-dependent:

```yaml
endpointRules:
  accounts@call:
    req-acc:
      - allow-complex-financial-check
    visibility:
      roles: manager teller
      groups: portal.w
```

When `visibility` is present, `tools/list` uses it instead of deriving
visibility from `permission` and known `req-acc` rule IDs. `tools/call` still
uses the configured `req-acc` rules.

The visibility check should normalize both permission values and JWT claim
values as string sets. It should accept either space-separated strings or
arrays. For example, the following should be treated as equivalent:

```json
{ "roles": "account-manager teller" }
```

```json
{ "roles": ["account-manager", "teller"] }
```

The standard dimensions are:

| Permission key | JWT claim keys |
|----------------|----------------|
| `roles` | `role`, `roles` |
| `groups` | `scp`, `grp`, `group`, `groups` |
| `positions` | `pos`, `position`, `positions` |
| `attributes` | `att`, `attribute`, `attributes` |
| `users` | `uid`, `user_id`, `sub` |

Claim lookup is against the same normalized claims map used by `req-acc` CEL:

```text
auditInfo.subject_claims.ClaimsMap
```

For example, the visibility checker resolves `roles` by reading
`auditInfo.subject_claims.ClaimsMap.role` or
`auditInfo.subject_claims.ClaimsMap.roles`, and resolves `groups` by reading
`auditInfo.subject_claims.ClaimsMap.scp`,
`auditInfo.subject_claims.ClaimsMap.grp`,
`auditInfo.subject_claims.ClaimsMap.group`, or
`auditInfo.subject_claims.ClaimsMap.groups`.

If a deployment uses non-standard claim names, add an explicit claim mapping
under `toolsListAccessControl`:

```yaml
toolsListAccessControl:
  mode: permission
  claimMappings:
    roles:
      - custom_roles
    groups:
      - custom_scope
```

When a mapping is present for a permission key, the mapped claim names are used
instead of the standard aliases for that key. Keys without a mapping continue to
use the standard aliases.

This covers the sample policy where:

- `local_mcp_echo` is visible to `account-manager`.
- `local_mcp_get_random_number` is visible to `category-admin`.
- OpenAPI tools protected by `allow-scp-claim-group-access-control.lightapi.net`
  are visible when the caller has `portal.w` in `scp`.

## Rule Awareness

The visibility filter should inspect the endpoint's `req-acc` rule IDs and use
`accessRuleLogic` to combine known checks:

```json
{
  "req-acc": [
    "allow-role-based-access-control.lightapi.net",
    "allow-scp-claim-group-access-control.lightapi.net"
  ]
}
```

For known generic rules:

- `allow-role-based-access-control.lightapi.net` maps to `permission.roles`
  against role claims.
- `allow-scp-claim-group-access-control.lightapi.net` maps to
  `permission.groups` against group or scope claims.

If `accessRuleLogic` is `any`, one known rule match makes the tool visible. If
it is `all`, every known rule must match.

Unknown custom `req-acc` rules need a configured fallback. The safer default is
to hide the tool in `permission` mode unless explicit `visibility` metadata is
present:

```yaml
endpointRules:
  accounts@call:
    req-acc:
      - allow-custom-account-access
    visibility:
      groups: portal.w
```

This avoids accidentally exposing tools protected by custom call-time logic.

Rules that do not authorize access should be marked so list visibility can
ignore them:

```yaml
ruleBodies:
  request-correlation-logger:
    ruleId: request-correlation-logger
    ruleType: req-acc
    accessControlEffect: telemetry
```

The list visibility checker should ignore rules whose
`accessControlEffect` is `telemetry` or `none`. Rules without an explicit effect
are treated as authorizing rules.

## Default Deny And Missing Rules

The list visibility fallback should mirror `tools/call` fallback behavior:

| Policy state | `defaultDeny: true` | `defaultDeny: false` |
|--------------|---------------------|----------------------|
| No endpoint rule | Hidden | Visible |
| Endpoint rule with no `req-acc` | Hidden | Visible |
| Endpoint rule with known `req-acc` | Visible only when permission matches | Visible only when permission matches |
| Endpoint rule with unknown `req-acc` | Hidden unless list-specific metadata allows it | Hidden unless list-specific metadata allows it |

This keeps issue-165 behavior consistent: `defaultDeny: false` can expose tools
without requiring no-op rules, while configured access rules still control
tools that have policy.

If an endpoint has explicit `visibility` metadata, that metadata decides list
visibility regardless of `defaultDeny`. `defaultDeny` only applies when no
endpoint rule or no request-access/list-visibility policy is available.

## skipPathPrefixes

The MCP router already treats `skipPathPrefixes` as matching either the MCP
tool name or the endpoint key. `tools/list` should use the same behavior:

```yaml
skipPathPrefixes:
  - local_mcp
```

With this configuration, tools such as `local_mcp_echo` and
`local_mcp_get_random_number` are visible and callable without access-control
evaluation, even when their endpoint keys are `echo@call` and
`getRandomNumber@call`.

## CEL Mode

`cel` mode can be useful when an operator wants the list to follow the exact
configured `req-acc` expressions and accepts the cost.

In this mode, the router evaluates `req-acc` for each candidate tool with:

```json
{
  "toolArguments": {}
}
```

This mode should be documented as argument-insensitive. Rules that require
specific `toolArguments` are not reliable for list visibility. `tools/call`
remains authoritative.

CEL mode must fail closed. If a `req-acc` rule fails to evaluate during
`tools/list`, the tool is hidden and the gateway logs a debug or warn event with
the rule ID, endpoint, and tool name. This makes argument-dependent rules
visible to operators without exposing tools whose list-time authorization could
not be proven.

CEL mode also needs a scale guard. The router should stop evaluating list-time
CEL after `maxCelEvaluations` candidate tools and hide the remaining unevaluated
tools, or reject the `tools/list` request with a clear configuration error. The
preferred default is to hide unevaluated tools and log a warning.

## Query Filtering Order

The router already supports `tools/list` query filtering. The recommended order
is:

1. Start from configured tools.
2. Apply the query or intent filter.
3. Apply list visibility.
4. Return the filtered MCP tools array.

Applying the query first reduces the number of authorization checks without
changing the response semantics, because hidden tools are never returned.

The query value used for filtering and caching must be normalized before it is
included in a cache key. At minimum, trim whitespace and lowercase the query. If
the router later accepts structured query parameters, sort the parameter names
and normalize repeated whitespace before hashing.

## Caching

The visibility result is cached per gateway process when
`toolsListAccessControl.mode` is not `none` and `maxCacheEntries` is greater
than zero. The cache key includes:

- Authenticated principal identity, such as `uid`, `sub`, or `client_id`.
- A stable hash of the normalized claims map used by visibility checks, or the
  token signature when available. Do not key only by user ID, because a user's
  roles or scopes can change between tokens.
- A stable hash of normalized request headers, because CEL mode can inspect
  headers as part of `req-acc` evaluation.
- Normalized query string.

The cache is a size-bounded LRU cache. The default maximum is 2000 entries per
gateway process and can be changed with `maxCacheEntries`. Setting
`maxCacheEntries: 0` disables the cache. The MCP router runtime does not carry
this cache across reloads, so MCP router, access-control, and rule reloads
naturally invalidate cached visibility results.

The cache implementation must protect the gateway from high-cardinality query
strings generated by agents. The LRU bound limits memory growth; highly unique
queries will evict older entries instead of growing the cache without bound.

## Security Notes

`tools/list` filtering improves agent ergonomics and reduces accidental tool
selection. It must not be treated as the security boundary.

The security boundary remains `tools/call`:

- `req-acc` runs before every downstream tool call.
- `res-fil` runs after eligible downstream responses.
- Argument-dependent authorization belongs in `tools/call`, not `tools/list`.

The design should therefore prefer fast, conservative list filtering and keep
full rule evaluation on invocation.
