# Request Access Control Rules

Light Gateway provides a fine-grained access control layer that evaluates
[CEL](https://cel.dev/) expressions against incoming JWT token claims and
per-endpoint permissions. Rules are evaluated at request time, before the
request reaches the upstream service, allowing you to enforce least-privilege
access without modifying the backend.

---

## Overview

The rule engine is driven by two configuration keys, both of which can live in
`values.yml` (or any merged config layer):

| Key | Purpose |
|-----|---------|
| `rule.endpointRules` | Maps each endpoint ID to one or more rule IDs and an optional `permission` object. |
| `rule.ruleBodies`    | Contains the full rule definition, including the CEL expression to evaluate. |

When a request arrives at an MCP or REST endpoint, the gateway:

1. Looks up the endpoint's `req-acc` rule list in `rule.endpointRules`.
2. Extracts the `permission` object (if any) from that endpoint's entry.
3. Builds a CEL evaluation context that includes the decoded JWT claims
   (`auditInfo`), the endpoint permission object (`permission`), request
   headers, and tool arguments.
4. Evaluates each rule's `expression` against that context.
5. Allows the request only if **all** rules return `true`.

---

## CEL Evaluation Context

The following top-level variables are available in every CEL expression:

| Variable | Type | Description |
|----------|------|-------------|
| `auditInfo` | map | Decoded JWT claims and correlation metadata. |
| `permission` | map | The per-endpoint permission object from `endpointRules`. |
| `headers` | map | Normalised (lowercased) HTTP request headers. |
| `toolName` | string | MCP tool name (for MCP endpoints). |
| `toolArguments` | map | MCP tool call arguments (for MCP endpoints). |
| `endpoint` | string | Endpoint identifier (`<name>@<method>`). |
| `correlationId` | string | Request correlation ID (may be absent). |

### JWT Claims inside `auditInfo`

JWT claims are nested at `auditInfo.subject_claims.ClaimsMap`.  The gateway
also promotes a small set of well-known fields as top-level entries in that
map:

| CEL path | JWT source | Type |
|----------|------------|------|
| `auditInfo.subject_claims.ClaimsMap.scp` | `scp` claim | `list<string>` or `string` |
| `auditInfo.subject_claims.ClaimsMap.roles` | `roles` claim | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.positions` | `positions` claim | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.attributes` | `attributes` claim | `map<string,string>` |
| `auditInfo.subject_claims.ClaimsMap.groups` | `groups` claim | `list<string>` |
| `auditInfo.subject_claims.ClaimsMap.sub` | `sub` claim | `string` |
| `auditInfo.subject_claims.ClaimsMap.client_id` | `client_id` / `azp` claim | `string` |
| `auditInfo.subject_claims.ClaimsMap.uid` | user ID injected by gateway | `string` |
| `auditInfo.subject_claims.ClaimsMap.role` | `role` claim (singular) | `string` |

> **Tip:** Always guard list membership with an `in` check before accessing a
> key. Claims that are absent from the token will be missing from `ClaimsMap`,
> and an unguarded access will cause the CEL expression to return an error
> (which is treated as `denied`).

---

## Configuration Structure

### `rule.endpointRules`

```yaml
rule.endpointRules:
  "<endpointId>":
    req-acc:
      - "<ruleId>"
    permission:
      <permissionKey>: <permissionValue>
```

- `<endpointId>` — The tool/endpoint name plus its method, e.g.
  `getRandomNumber@call` for an MCP tool call or `getPetById@get` for a REST
  `GET` endpoint.
- `req-acc` — List of rule IDs that are evaluated as *request access* rules.
- `permission` — Arbitrary key/value map injected into the CEL context as
  `permission`. This is where you define *what* the endpoint requires (a
  specific group, role, etc.) without hard-coding it into the rule body.

### `rule.ruleBodies`

```yaml
rule.ruleBodies:
  "<ruleId>":
    ruleId: "<ruleId>"
    ruleName: "Human-readable name"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    expression: |
      <CEL expression that returns bool>
    author: "<userId>"
    hostId: "<hostId>"
    version: "1.0.0"
    common: "Y"
    actions: []
```

`conditionSecurityProfile: "strict"` is strongly recommended. The strict
profile limits which functions and variables are accessible in order to prevent
information leakage and code injection.

---

## Permission Types and CEL Examples

All examples below use the same reusable rule body.  You only need to change
the `permission` map in `endpointRules` per endpoint.

### 1. Scope (`scp`) — OAuth 2.0 Access Token

OAuth 2.0 `scp` claims may arrive as a JSON array (recommended) or as a
space-delimited string (legacy). The examples below handle the array form.

**Rule body (define once):**

```yaml
rule.ruleBodies:
  "allow-scp-group.lightapi.net":
    ruleId: "allow-scp-group.lightapi.net"
    ruleName: "Allow request when scp claim contains the required group"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'scp' in auditInfo.subject_claims.ClaimsMap
      && 'groups' in permission
      && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "getRandomNumber@call":
    req-acc:
      - "allow-scp-group.lightapi.net"
    permission:
      groups: "portal.w"

  "listReports@call":
    req-acc:
      - "allow-scp-group.lightapi.net"
    permission:
      groups: "report.r"
```

Each endpoint simply declares which value the `groups` key must match; the
rule body stays generic.

---

### 2. Roles

Use when your identity provider issues a `roles` array claim (common in
Microsoft Entra ID / Azure AD).

**Rule body:**

```yaml
rule.ruleBodies:
  "allow-role.lightapi.net":
    ruleId: "allow-role.lightapi.net"
    ruleName: "Allow request when roles claim contains the required role"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'roles' in auditInfo.subject_claims.ClaimsMap
      && 'role' in permission
      && permission.role in auditInfo.subject_claims.ClaimsMap.roles
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "adminAction@call":
    req-acc:
      - "allow-role.lightapi.net"
    permission:
      role: "ADMIN"

  "viewDashboard@call":
    req-acc:
      - "allow-role.lightapi.net"
    permission:
      role: "VIEWER"
```

**Sample JWT `roles` claim:**

```json
{
  "sub": "user@example.com",
  "roles": ["VIEWER", "EDITOR"],
  "scp": ["openid", "profile"]
}
```

The `viewDashboard@call` rule would pass because `"VIEWER"` is in the `roles`
array; `adminAction@call` would be denied.

---

### 3. Positions

Use when your organization assigns positional clearances (e.g., department or
job level) that are issued as a `positions` claim.

**Rule body:**

```yaml
rule.ruleBodies:
  "allow-position.lightapi.net":
    ruleId: "allow-position.lightapi.net"
    ruleName: "Allow request when positions claim contains the required position"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'positions' in auditInfo.subject_claims.ClaimsMap
      && 'position' in permission
      && permission.position in auditInfo.subject_claims.ClaimsMap.positions
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "approveExpense@call":
    req-acc:
      - "allow-position.lightapi.net"
    permission:
      position: "MANAGER"

  "submitExpense@call":
    req-acc:
      - "allow-position.lightapi.net"
    permission:
      position: "EMPLOYEE"
```

**Sample JWT `positions` claim:**

```json
{
  "sub": "alice@example.com",
  "positions": ["EMPLOYEE", "TEAM_LEAD"]
}
```

---

### 4. Attributes

Use when access control depends on a key/value attribute map (e.g., department,
clearance level, region).

**Rule body:**

```yaml
rule.ruleBodies:
  "allow-attribute.lightapi.net":
    ruleId: "allow-attribute.lightapi.net"
    ruleName: "Allow request when a specific attribute matches the required value"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'attributes' in auditInfo.subject_claims.ClaimsMap
      && 'attributeKey' in permission
      && 'attributeValue' in permission
      && permission.attributeKey in auditInfo.subject_claims.ClaimsMap.attributes
      && auditInfo.subject_claims.ClaimsMap.attributes[permission.attributeKey] == permission.attributeValue
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "viewFinancials@call":
    req-acc:
      - "allow-attribute.lightapi.net"
    permission:
      attributeKey: "department"
      attributeValue: "FINANCE"

  "viewEngineering@call":
    req-acc:
      - "allow-attribute.lightapi.net"
    permission:
      attributeKey: "department"
      attributeValue: "ENGINEERING"
```

**Sample JWT `attributes` claim:**

```json
{
  "sub": "bob@example.com",
  "attributes": {
    "department": "FINANCE",
    "clearance": "LEVEL_2"
  }
}
```

---

### 5. Combined Claims (AND logic)

Combine multiple claim checks in a single expression when an endpoint requires
*all* conditions to be satisfied simultaneously.

**Example: require both a role and a specific scope**

```yaml
rule.ruleBodies:
  "allow-role-and-scp.lightapi.net":
    ruleId: "allow-role-and-scp.lightapi.net"
    ruleName: "Allow request when caller has both the required role and scope"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'roles' in auditInfo.subject_claims.ClaimsMap
      && 'scp' in auditInfo.subject_claims.ClaimsMap
      && 'role' in permission
      && 'groups' in permission
      && permission.role in auditInfo.subject_claims.ClaimsMap.roles
      && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "deleteRecord@call":
    req-acc:
      - "allow-role-and-scp.lightapi.net"
    permission:
      role: "ADMIN"
      groups: "data.delete"
```

---

### 6. OR logic — Multiple Rules

To implement *any-of* semantics, assign multiple rules to the same endpoint and
write each rule to cover one accepted path.  The gateway allows the request if
**all** assigned rules return `true`, so for OR logic write a **single** rule
with an `||` operator:

```yaml
rule.ruleBodies:
  "allow-admin-or-auditor.lightapi.net":
    ruleId: "allow-admin-or-auditor.lightapi.net"
    ruleName: "Allow request for ADMIN or AUDITOR role"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'roles' in auditInfo.subject_claims.ClaimsMap
      && (
        'ADMIN' in auditInfo.subject_claims.ClaimsMap.roles
        || 'AUDITOR' in auditInfo.subject_claims.ClaimsMap.roles
      )
```

---

### 7. Subject / User ID

Use the `sub` claim to restrict an endpoint to a specific user (for
impersonation endpoints, personal resources, etc.).

```yaml
rule.ruleBodies:
  "allow-subject.lightapi.net":
    ruleId: "allow-subject.lightapi.net"
    ruleName: "Allow request only for the configured subject"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'sub' in auditInfo.subject_claims.ClaimsMap
      && 'subject' in permission
      && auditInfo.subject_claims.ClaimsMap.sub == permission.subject
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "getMyProfile@call":
    req-acc:
      - "allow-subject.lightapi.net"
    permission:
      subject: "service-account-ci@project.iam.gserviceaccount.com"
```

---

### 8. Client ID (Machine-to-Machine)

For M2M flows where no user is present, restrict by the OAuth 2.0 client ID
(`client_id` or `azp` claim).

```yaml
rule.ruleBodies:
  "allow-client.lightapi.net":
    ruleId: "allow-client.lightapi.net"
    ruleName: "Allow request only for the configured OAuth client"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      'client_id' in auditInfo.subject_claims.ClaimsMap
      && 'clientId' in permission
      && auditInfo.subject_claims.ClaimsMap.client_id == permission.clientId
```

**Endpoint binding:**

```yaml
rule.endpointRules:
  "ingestMetrics@call":
    req-acc:
      - "allow-client.lightapi.net"
    permission:
      clientId: "data-pipeline-client"
```

---

## Defining a Fully Generic Rule

A single reusable rule can cover any claim type by inspecting what keys are
present in `permission`.  This approach minimises the number of rule bodies you
need to maintain.

```yaml
rule.ruleBodies:
  "allow-dynamic-claim.lightapi.net":
    ruleId: "allow-dynamic-claim.lightapi.net"
    ruleName: "Dynamic claim-based access control driven entirely by endpoint permission"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    actions: []
    expression: |
      (
        !('groups' in permission)
        || (
          'scp' in auditInfo.subject_claims.ClaimsMap
          && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
        )
      )
      && (
        !('role' in permission)
        || (
          'roles' in auditInfo.subject_claims.ClaimsMap
          && permission.role in auditInfo.subject_claims.ClaimsMap.roles
        )
      )
      && (
        !('position' in permission)
        || (
          'positions' in auditInfo.subject_claims.ClaimsMap
          && permission.position in auditInfo.subject_claims.ClaimsMap.positions
        )
      )
```

With this rule you can mix and match checks at the endpoint level:

```yaml
rule.endpointRules:
  # Only scp group required
  "echo@call":
    req-acc:
      - "allow-dynamic-claim.lightapi.net"
    permission:
      groups: "portal.r"

  # scp group AND role required
  "adminTool@call":
    req-acc:
      - "allow-dynamic-claim.lightapi.net"
    permission:
      groups: "admin.w"
      role: "ADMIN"

  # position only
  "approvePayroll@call":
    req-acc:
      - "allow-dynamic-claim.lightapi.net"
    permission:
      position: "MANAGER"
```

---

## Complete `values.yml` Example

The following snippet shows a complete, self-contained configuration for a
gateway that protects two MCP tools using scope-based group checks.

```yaml
# Bind rules to endpoints
rule.endpointRules:
  "echo@call":
    req-acc:
      - "allow-scp-group.lightapi.net"
    permission:
      groups: "portal.r"

  "getRandomNumber@call":
    req-acc:
      - "allow-scp-group.lightapi.net"
    permission:
      groups: "portal.w"

# Define the reusable rule body
rule.ruleBodies:
  "allow-scp-group.lightapi.net":
    ruleId: "allow-scp-group.lightapi.net"
    ruleName: "Allow request when scp claim contains the required group"
    ruleType: "req-acc"
    conditionLanguage: "cel"
    conditionSecurityProfile: "strict"
    version: "1.0.0"
    common: "Y"
    author: "<your-user-id>"
    hostId: "<your-host-id>"
    actions: []
    expression: |
      'scp' in auditInfo.subject_claims.ClaimsMap
      && 'groups' in permission
      && permission.groups in auditInfo.subject_claims.ClaimsMap.scp
```

A matching JWT for `getRandomNumber@call` would include:

```json
{
  "sub": "user@example.com",
  "scp": ["openid", "profile", "portal.w"],
  "iat": 1718300000,
  "exp": 1718303600
}
```

---

## CEL Expression Guidelines

### Always guard before accessing

Never access a claim key without first confirming it exists in the map:

```cel
# WRONG — will error if 'roles' is not present in the token
permission.role in auditInfo.subject_claims.ClaimsMap.roles

# CORRECT
'roles' in auditInfo.subject_claims.ClaimsMap
&& permission.role in auditInfo.subject_claims.ClaimsMap.roles
```

### Avoid `.split()` on array claims

OAuth `scp` can be issued as a JSON array.  Do **not** call `.split(' ')` on
it; use the `in` operator directly:

```cel
# WRONG — split() fails on a JSON array
auditInfo.subject_claims.ClaimsMap.scp.split(' ').exists(w, w == 'portal.w')

# CORRECT — works for both array and list types
'portal.w' in auditInfo.subject_claims.ClaimsMap.scp
```

### Use `strict` security profile

Always set `conditionSecurityProfile: "strict"` in the rule body. The strict
profile disables macros and functions that could leak internal state or be
abused for injection attacks.

### Keep expressions readable

Long expressions should use line breaks and parentheses for clarity.  CEL
ignores whitespace, so multi-line expressions in YAML block scalars (`|`) are
perfectly valid.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| `policyOutcome=denied` with no error | Rule expression returned `false` | Check that the token includes the required claim value |
| `policyOutcome=denied` with `Undeclared reference` | Used a variable not in the strict allowlist | Use only `auditInfo`, `permission`, `headers`, `toolName`, `toolArguments`, `endpoint`, `correlationId` |
| `policyOutcome=denied` with `no such key` | Accessed a missing claim without a guard | Add `'<key>' in auditInfo.subject_claims.ClaimsMap &&` before the access |
| Rule never fires | `endpointRules` key does not match the actual endpoint ID | Check the `endpoint` field in the gateway logs for the exact ID |
| All requests allowed unexpectedly | `req-acc` list is empty or the rule ID has a typo | Verify `rule.endpointRules` references the exact `ruleId` from `rule.ruleBodies` |
