# MCP Tools Access Control

`light-gateway` can enforce fine-grained access control for MCP tools exposed
through the MCP router. The router uses the shared access-control runtime from
`light-pingora`, so MCP tools use the same `access-control.yml` and `rule.yml`
policy files as HTTP API access control.

For MCP traffic, rules apply only to `tools/call` requests:

- `req-acc` rules run before the downstream HTTP or MCP tool is called.
- `res-fil` rules run after the downstream response is converted to an MCP
  result and before the JSON-RPC response is returned to the agent.

`tools/list`, `initialize`, `notifications/initialized`, and session
management requests are handled by the MCP router and are not authorized as
individual business tools.

## MCP Router And access-control.enabled

The `access-control.enabled` flag is the top-level switch for MCP tool access
control:

```yaml
enabled: true
accessRuleLogic: any
defaultDeny: true
defaultInclude: false
skipPathPrefixes: []
```

When `access-control.enabled` is `true`, the MCP router evaluates configured
`req-acc` rules before invoking a tool. If the tool call is allowed and the
matching endpoint has `res-fil` rules, the router also applies response row or
column filters before returning the MCP result.

`skipPathPrefixes` bypasses the same two phases for matching endpoint keys.
For example, if `skipPathPrefixes` contains `accounts`, then an `accounts@call`
tool endpoint is allowed without `req-acc` evaluation and its result is returned
without `res-fil` filtering.

When `access-control.enabled` is `false`, the MCP router bypasses both phases:

- `req-acc` rules do not deny MCP tool calls.
- `res-fil` rules do not alter MCP tool results.

This bypass applies even when `rule.yml` is still present and contains matching
endpoint rules. The rules can remain loaded for later re-enable or reload, but
the disabled access-control switch prevents the MCP router from enforcing
authorization or response filtering.

This setting is independent from `mcp-router.enabled`. Set
`mcp-router.enabled: false` to disable the MCP endpoint itself. Set
`access-control.enabled: false` only when the MCP endpoint should continue to
serve tools without access-control enforcement.

## Endpoint Rules

Each MCP tool maps to a stable endpoint key. If the tool config contains an
explicit `endpoint`, that value is used. Otherwise, the router derives a key
from the tool name and method, such as `accounts@call`.

```yaml
tools:
  - name: accounts
    description: List accounts
    targetHost: http://account-api:8080
    path: /accounts
    method: GET
    endpoint: accounts@call
    apiType: http
```

The same endpoint key is referenced from `rule.yml`:

```yaml
endpointRules:
  accounts@call:
    req-acc:
      - allow-account-reader
    res-fil:
      - filter-account-rows
      - filter-account-columns
    permission:
      roles: teller manager
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: C
      col:
        role:
          teller: accountNo,accountType,balance
```

## Request Authorization

A `req-acc` rule decides whether the MCP tool call can proceed. When
`defaultDeny` is `true`, a tool call with no matching endpoint rule or no
`req-acc` rules is denied.

```yaml
ruleBodies:
  allow-account-reader:
    common: Y
    ruleId: allow-account-reader
    ruleName: Allow account reader
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      auditInfo.subject_claims.ClaimsMap.role in ["teller", "manager"]
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
```

## Response Filtering

A `res-fil` rule transforms the MCP tool result after the downstream call
succeeds. Row filters and column filters operate on the JSON payload carried in
the MCP result `structuredContent` and mirrored text content.

```yaml
ruleBodies:
  filter-account-rows:
    common: Y
    ruleId: filter-account-rows
    ruleName: Filter account rows
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction

  filter-account-columns:
    common: Y
    ruleId: filter-account-columns
    ruleName: Filter account columns
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
```

`defaultInclude` affects row filtering when no caller claim matches a configured
row-filter entry. Keep it `false` to fail closed and return no rows. Set it to
`true` only when the desired compatibility behavior is to keep all rows.
