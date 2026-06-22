# CEL Rule Conditions

Light-Fabric supports CEL rule conditions only. A Light-Fabric rule uses
`conditionLanguage: cel` and one rule-level CEL boolean `expression`.

The old native condition schema with condition rows, operators, and `joinCode`
is a legacy Java yaml-rule format. It can still be documented for migration and
Java compatibility, but it is not a supported Light-Fabric runtime condition
format.

Each Light-Fabric rule should therefore use CEL. Mixing native condition rows and
CEL expressions inside the same rule is not a canonical model because it makes
portal authoring, validation, and runtime dispatch harder to reason about.

## Goals

- support CEL expressions as the Light-Fabric rule-level condition language
- reuse the existing rule context for gateway, workflow, and test execution
- preserve existing `actions`, `endpointRules`, and rule phase semantics
- let Light-Portal choose the correct editor from rule metadata without parsing
  arbitrary rule bodies
- validate CEL before publishing or reloading rules where possible
- keep CEL execution deterministic and side-effect free

## Non-Goals

- replacing actions with CEL
- allowing CEL expressions to perform I/O, network calls, mutation, or service
  lookups
- allowing CEL expressions to directly mutate `responseBody` or perform general
  JSON transformations
- supporting the legacy Java yaml-rule native condition-row format in
  Light-Fabric
- supporting mixed native and CEL condition blocks in the canonical portal
  authoring flow

## Current Model

The legacy Java yaml-rule model contains an optional flat list of native
conditions:

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
```

Each legacy native condition contains:

- `operator`
- `operand`
- `expected`
- `joinCode`

The Java yaml-rule engine evaluates conditions left-to-right. `joinCode`
combines each condition with the accumulated result. This format is shown here
only as migration context.

Portal persistence stores rule metadata in `rule_t` and the executable rule JSON
in `rule_t.rule_body`. Today there is no dedicated column that tells the portal
which condition editor to render, so the UI would have to inspect `rule_body`.

## Proposed Rule Shape

Use a rule-level condition language flag. Light-Fabric accepts `cel` for a
single CEL expression. `native` is reserved for legacy Java yaml-rule data and
must not be emitted to Light-Fabric runtime configuration.

Persist the flag in both places:

- `rule_t.condition_language`: indexed/listable portal metadata
- `ruleBody.conditionLanguage`: self-contained exported runtime configuration

Recommended Light-Fabric value:

```text
cel
```

Legacy Java yaml-rule native rule body:

```yaml
ruleBodies:
  allowMcpReader:
    common: Y
    ruleId: allowMcpReader
    ruleName: Allow MCP reader
    ruleType: req-acc
    conditionLanguage: native
    conditions:
      - operatorCode: isNotNull
        propertyPath: auditInfo.subject_claims.ClaimsMap.role
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
```

CEL rule body:

```yaml
ruleBodies:
  allowApprovedTransfer:
    common: Y
    ruleId: allowApprovedTransfer
    ruleName: Allow approved transfer
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      auditInfo.subject_claims.ClaimsMap.role != null
      && roles.exists(r, r == auditInfo.subject_claims.ClaimsMap.role)
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
```

Recommended database shape:

```sql
ALTER TABLE rule_t
ADD COLUMN condition_language VARCHAR(16) DEFAULT 'cel' NOT NULL;

ALTER TABLE rule_t
ADD COLUMN condition_security_profile VARCHAR(32);

ALTER TABLE rule_t
ADD CONSTRAINT rule_t_condition_language_check
CHECK (condition_language IN ('cel'));

ALTER TABLE rule_t
ADD CONSTRAINT rule_t_condition_security_profile_check
CHECK (
  condition_security_profile IS NULL
  OR condition_security_profile IN ('strict', 'standard', 'internal-admin')
);
```

Recommended schema rules:

- `conditionLanguage` is optional and defaults to `cel`
- `conditionLanguage: cel` requires `expression` and rejects `conditions`
- `conditionSecurityProfile` is optional and names a runtime-defined profile
- `conditionLanguage: native` is rejected by Light-Fabric runtime config
- unknown rule and condition fields should continue to be rejected by the schema
- command handlers should reject requests where the DB metadata and rule body
  condition language disagree

This can be represented with conditional validation in
`rule-specification/schema/rule.yaml`:

```yaml
allOf:
  - if:
      properties:
        conditionLanguage:
          const: cel
      required: [conditionLanguage]
    then:
      required: [expression]
      not:
        required: [conditions]
    else:
      properties:
        conditionLanguage:
          const: cel
```

The Rust model can add optional fields to `Rule`:

```rust
pub condition_language: Option<String>,
pub condition_security_profile: Option<String>,
pub expression: Option<String>,
```

This is less disruptive than changing `RuleCondition` into an enum and keeps old
rule bodies valid.

## Cross-Repository Scope

This change crosses the rule specification, runtime engines, portal services, and
portal UI. The implementation should be tracked as a coordinated change rather
than a `light-fabric`-only feature.

| Area | Required work |
| --- | --- |
| `rule-specification` | Add `conditionLanguage`, `conditionSecurityProfile`, `expression`, CEL rule schema validation, and explicit rejection of native condition rows for Light-Fabric runtime config. |
| `portal-db` | Add `rule_t.condition_language` with default `cel`, optional `rule_t.condition_security_profile`, check constraints, and pending rule-change approval state if workflow task payloads are not sufficient. |
| `light-portal` | Update persistence and projection code so rule create/update/read/export/import paths carry `conditionLanguage` and `conditionSecurityProfile`; ensure endpoint rule config generation emits only approved, self-contained rule bodies; integrate stronger-profile requests with worklist and assistant-task approval. |
| `rule-command` | Accept `conditionLanguage`, `conditionSecurityProfile`, and `expression`, reject native condition-row payloads for Light-Fabric rules, validate mode/profile-specific shape, publish `strict` changes immediately, route stronger profile requests through approval, and write both DB metadata and rule body consistently after approval. |
| `rule-query` | Return `conditionLanguage`, `conditionSecurityProfile`, and approval status for list/detail APIs, include selected/effective profiles in test-case execution payloads, and surface CEL parse/type/missing-field/profile errors from Java and Rust runners. |
| `portal-view` | Render the CEL expression editor for Light-Fabric rules; keep any native condition builder scoped to legacy Java yaml-rule authoring; show a controlled profile selector for CEL rules; submit `strict` directly and route `standard` or `internal-admin` to worklist approval; do not require the UI to infer mode from `ruleBody`. |
| workflow and assistant task | Use the existing human-in-the-loop worklist flow for stronger profile approval, route tasks to `admin` and `rule-admin`, and attach an advisory assistant-task risk summary for the approver. |
| `light-fabric` | Add `conditionLanguage`, `conditionSecurityProfile`, and `expression` to `crates/light-rule`, dispatch in `RuleEngine`, add policy-driven CEL evaluator/caching, and update gateway/workflow tests. |
| `yaml-rule` | Add Java runtime parity for `conditionLanguage: cel` and named profile enforcement if Java services need to execute the same rules; otherwise reject CEL rules explicitly with a clear runtime-capability error. |

`portal-db` is listed even though it is not a rule engine because `rule_t` lives
there. Without the DB column, `portal-view` would need to parse the compact rule
body to choose the editor, which is the coupling this design is trying to avoid.

## Operator Alias Alternative

Another possible shape is to add `operatorCode: cel` and store the CEL
expression in `expected` inside `conditions`:

```yaml
conditions:
  - operatorCode: cel
    expected: >
      context.toolArguments.amount < 1000
      || roles.exists(r, r == "approver")
```

This has one advantage for legacy Java yaml-rule imports: `operator`, `operand`,
and `expected` already exist. It is not useful as the Light-Fabric runtime
contract because Light-Fabric does not support native condition rows.

It should not be the canonical schema because:

- CEL is a full boolean expression, not a comparison operator
- overloading `expected` makes validation and portal rendering less clear
- `operand` becomes ignored or artificial
- the UI still has to draw a condition-row editor even though the rule is really
  a single expression
- future expression languages would continue overloading legacy native condition
  fields

The recommended contract is therefore:

- canonical form: `conditionLanguage: cel` plus rule-level `expression`
- reject `operatorCode: cel` for Light-Fabric runtime config
- normalize any legacy import to the canonical rule-level CEL model before
  persistence or runtime export

## Mixed Conditions Alternative

Another possible shape is to allow native and CEL conditions in the same
`conditions` array. Light-Fabric should not support this. Native condition rows
belong to the legacy Java yaml-rule model only.

Reasons to avoid canonical mixed rules:

- Light-Portal would need a hybrid editor that switches row-by-row
- validation errors become harder to explain to non-technical users
- `joinCode` semantics across native and CEL expressions are correct but subtle
- users may expect CEL operator precedence inside the whole rule even though
  native `joinCode` remains left-to-right
- runtime dispatch is simpler and faster when the rule selects one evaluator

If mixed rules are accepted from an import path, they must be normalized to a
single rule-level CEL expression before they are persisted or exported to
Light-Fabric runtime configuration.

## Execution Model

Rule execution should dispatch by `conditionLanguage` once per rule:

```text
RuleEngine::execute_rule
  -> conditionLanguage == cel
     -> evaluate rule expression
  -> execute actions when conditions pass
```

The outer behavior stays unchanged:

- rules with no conditions continue to run actions
- CEL rules without an expression fail validation before runtime
- failed conditions skip actions
- failed action execution fails the rule
- endpoint rule ordering and access-control logic stay unchanged
- `req-tra` and `res-tra` continue to run sequentially
- access-control rules can still be evaluated independently

Runtime should treat a missing `conditionLanguage` as `cel` only when an
`expression` is present. Native condition-row payloads must be rejected by
Light-Fabric config validation.

## CEL And Response Filtering

CEL is the rule predicate language, not the response mutation engine. A rule-level
CEL expression decides whether a `res-fil` rule applies. The response body is
then transformed by a standard action.

This keeps the contract narrow:

- CEL expressions are side-effect free and return booleans.
- Actions own mutation of `responseBody`.
- Actions decide how JSON arrays, JSON objects, and malformed payloads are
  handled.
- Actions provide stable audit and failure behavior.
- The runtime can compile and cache rule-level CEL independently from
  response-body parsing.
- The response-filter pipeline parses JSON once, lets actions mutate the same
  in-memory value, and serializes once after all `res-fil` actions complete.
- The parsed mutable response value is action-owned state and must not be
  exposed as a rule-level CEL mutation target.

The default response-filter actions should stay declarative:

- `ResponseRowFilterAction`: applies permission-defined row filters.
- `ResponseColumnFilterAction`: applies permission-defined column keep or remove
  lists.

Rule bodies should keep using the `actions[].actionClassName` field even when
the runtime is Rust. In Rust this value is not a Java class name. It is a stable
action registry key that selects the Rust action implementation. The Rust
gateway registers both the Java-compatible fully qualified names and short
aliases:

- `com.networknt.rule.ResponseRowFilterAction`
- `ResponseRowFilterAction`
- `com.networknt.rule.ResponseColumnFilterAction`
- `ResponseColumnFilterAction`
- `com.networknt.rule.ResponseCelRowFilterAction`
- `ResponseCelRowFilterAction`

For portal-authored and exported rules, prefer the fully qualified
Java-compatible names. They preserve compatibility with existing yaml-rule
configuration, schemas, import/export flows, and any Java runtime that reads the
same rule bodies:

```yaml
ruleBodies:
  rowFilterByJwtClaims:
    common: Y
    ruleId: rowFilterByJwtClaims
    ruleName: Row filter by JWT claims
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      row != null
      && (
        ("role" in row && "role" in auditInfo.subject_claims.ClaimsMap)
        || ("group" in row
            && ("grp" in auditInfo.subject_claims.ClaimsMap
                || "group" in auditInfo.subject_claims.ClaimsMap))
        || ("position" in row
            && ("pos" in auditInfo.subject_claims.ClaimsMap
                || "position" in auditInfo.subject_claims.ClaimsMap))
        || ("attribute" in row
            && ("att" in auditInfo.subject_claims.ClaimsMap
                || "attribute" in auditInfo.subject_claims.ClaimsMap))
      )
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction

  colFilterByJwtClaims:
    common: Y
    ruleId: colFilterByJwtClaims
    ruleName: Column filter by JWT claims
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      col != null
      && (
        ("role" in col && "role" in auditInfo.subject_claims.ClaimsMap)
        || ("group" in col
            && ("grp" in auditInfo.subject_claims.ClaimsMap
                || "group" in auditInfo.subject_claims.ClaimsMap))
        || ("position" in col
            && ("pos" in auditInfo.subject_claims.ClaimsMap
                || "position" in auditInfo.subject_claims.ClaimsMap))
        || ("attribute" in col
            && ("att" in auditInfo.subject_claims.ClaimsMap
                || "attribute" in auditInfo.subject_claims.ClaimsMap))
      )
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
```

The rule-level CEL expression only decides whether the response-filter action
runs. The action reads the endpoint `permission.row` or `permission.col`
configuration and matches it against JWT claims. The response-filter action
understands these permission dimensions:

- `role`: matched against the JWT `role` claim
- `group`: matched against `grp` or `group`
- `position`: matched against `pos` or `position`
- `attribute`: matched against `att` or `attribute`
- `user`: matched against `uid`, `user_id`, or `sub`

For example:

```yaml
endpointRules:
  /v1/accounts@get:
    res-fil:
      - rowFilterByJwtClaims
      - colFilterByJwtClaims
    permission:
      row:
        role:
          manager:
            - colName: status
              operator: =
              colValue: ACTIVE
        group:
          finance:
            - colName: department
              operator: =
              colValue: FIN
        position:
          director:
            - colName: level
              operator: ">="
              colValue: "5"
        attribute:
          region-east:
            - colName: region
              operator: =
              colValue: EAST
      col:
        role:
          manager: id,name,status,department
        group:
          finance: id,name,balance,status
        position:
          director: id,name,balance,status,level
        attribute:
          region-east: id,name,region,status
```

If a row predicate needs CEL, add an explicit CEL-aware action rather than
turning rule-level CEL into a JSON transformation DSL:

```yaml
ruleBodies:
  filterOfferRows:
    ruleId: filterOfferRows
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

`ResponseCelRowFilterAction` would compile `rowExpression` at rule-load time and
evaluate it once per candidate row with a curated context containing `row`,
`auditInfo`, `headers`, `endpoint`, `permission`, and request metadata. The
implementation should avoid deep-cloning the full base context for every row.
Use a child CEL context that shadows `row`, or reuse one mutable evaluation
context and replace only the `row` variable before each evaluation.

Row-level CEL evaluation errors should be row-local by default. If a row is
missing a referenced field or its predicate evaluation returns an error, drop
that row and emit debug or trace diagnostics with the rule id and field error.
Only configuration errors, such as an invalid `rowExpression` that fails to
compile, should fail the entire action closed.

Column filtering should remain declarative unless there is a proven use case for
dynamic column predicates. Role, group, attribute, user, and position based field
lists are easier to review, safer to render in Portal, and cheaper to execute
than arbitrary column-level CEL.

Column filtering must also support top-level JSON objects, not only arrays or
objects containing `items`. Single-object responses such as `GET /offers/123`
still need field hiding. Row filtering applies only to arrays or object payloads
with an `items` array.

## Rule Context

CEL should evaluate against the rule engine JSON context. For gateway
access-control and response filtering, this includes fields such as:

- `auditInfo`
- `headers`
- `endpoint`
- `toolName`
- `toolArguments`
- `correlationId`
- `responseBody`
- `statusCode`

Endpoint `permission` values are merged into the root context as their configured
keys. For example, `permission.roles` in `endpointRules` is available to
conditions as `roles`, response row filters are available as `row`, and column
filters are available as `col`. A future runtime can also expose a namespaced
`permission` object as an additive convenience, but CEL support should not
require that shape to preserve compatibility with existing actions.

For `standard` and `internal-admin` profiles, the CEL environment can expose
variables in two ways:

- top-level context fields as direct CEL variables, such as `auditInfo`,
  `toolArguments`, and `roles`
- the full root object as `context`, so expressions can use explicit paths such
  as `context.toolArguments.amount`

Direct variables keep expressions concise. The `context` variable is safer for
generated expressions, collision avoidance, and future fields that are not valid
CEL identifiers.

For the `strict` profile, the runtime should expose only curated root variables
such as `auditInfo`, `headers`, `toolArguments`, endpoint metadata, and
permission values needed by the rule phase. It should not expose the full
`context` object by default. This prevents future internal runtime metadata from
becoming visible to tenant-authored CEL just because it was appended to the root
request context.

The context contract should be documented as part of Light-Rule because CEL
expressions depend on stable field names. Adding fields is compatible. Renaming
or changing field shapes is a breaking change for CEL rules.

## Type Mapping

The CEL evaluator should receive deterministic values converted from
`serde_json::Value`:

- JSON object to CEL map
- JSON array to CEL list
- JSON string to CEL string
- JSON number to CEL integer or double
- JSON boolean to CEL bool
- JSON null to CEL null

Missing fields should evaluate according to the chosen CEL implementation's
standard behavior. The rule test API should expose these failures clearly so
authors can distinguish "expression false" from "expression invalid".

Authors should guard optional fields explicitly. Depending on the selected CEL
runtime and the field shape, this can use presence checks such as `has(...)` or
map membership checks such as:

```cel
"role" in auditInfo.subject_claims.ClaimsMap
  && auditInfo.subject_claims.ClaimsMap.role == "admin"
```

The portal rule tester should surface missing-field evaluation errors and suggest
guarded expressions instead of letting these failures look like ordinary denied
rules.

## Context Injection Performance

CEL expressions run on request paths, so context conversion must be controlled.
The implementation should not recursively deep-clone and convert large JSON
payloads separately for every CEL rule evaluation.

Recommended approach:

- compile expressions once at rule load
- build the rule context once per request or response phase
- reuse converted CEL variables across evaluations in the same request or
  response phase when possible
- prefer lazy or reference-backed variable resolution if the selected CEL crate
  supports it
- if eager conversion is required, convert only the variables exposed to CEL and
  avoid parsing large string fields such as `responseBody` unless an expression
  explicitly needs structured access to them
- for per-row CEL, avoid cloning the full base context for each row; use a child
  context or reusable mutable context that changes only the `row` binding
- benchmark access-control and response-filter scenarios before enabling CEL by
  default in high-throughput paths

The initial implementation can be pragmatic, but performance tests should guard
against accidentally making CEL expression evaluation proportional to the full
response body size when the expression only needs claims or endpoint metadata.

## Validation

CEL should be validated earlier than request execution.

Recommended validation points:

- portal rule editor
- rule command create/update handler
- rule test API
- runtime config reload

Validation must enforce the Light-Fabric rule shape:

- `cel`: `expression` is required, `conditions` is rejected
- `native`: rejected by Light-Fabric runtime config
- persisted `rule_t.condition_language` must match `ruleBody.conditionLanguage`
- persisted `rule_t.condition_security_profile` must match
  `ruleBody.conditionSecurityProfile` when either side is present

Runtime reload should reject invalid CEL when strict validation is enabled. If a
service must preserve availability, it can keep the last known-good rule set and
report the new config as rejected.

Approval workflow should not bypass validation. For profile escalation requests,
the command path should validate the submitted rule shape and expression before
creating the approval task. Final approval should revalidate the exact submitted
rule body before emitting the active rule event.

Validation output should include:

- rule id
- condition language
- parse or type error
- source offset when provided by the CEL implementation

## Compilation And Caching

Do not compile CEL on every request. Compile once per rule load and cache the
compiled program with the loaded rule set.

Recommended cache key:

```text
ruleId + expression hash + effective profile
```

The compiled expression cache should be replaced atomically when the rule config
reloads. It should not outlive the rule version it was compiled from. Old
compiled entries must be evicted during reload so repeated rule updates cannot
leak memory through stale expression hashes.

## Rust CEL Library

`cel-interpreter` is a practical first candidate for the Rust implementation. It
provides `Program::compile(...)`, `Program::execute(...)`, a `Context` for
variables and functions, and compiled `Program` values that are `Send + Sync`.

Implementation should still be isolated behind a small internal trait:

```text
CelEvaluator
  -> compile(ruleId, expression) -> compiled expression
  -> evaluate(compiled expression, serde_json::Value context) -> bool
```

This keeps Light-Rule from leaking third-party crate types through its public
model and allows the implementation to change if CEL crate maturity, feature
flags, or Java parity requirements change.

## Legacy Operator Migration

Legacy Java yaml-rule native conditions include operators that may not map
one-to-one to the selected CEL runtime. Examples include:

- `containsIgnoreCase`
- `matches` and `notMatch`
- `inList` and `notInList`
- `containsAny`, `containsAll`, and `containsNone`
- date-style comparisons such as `before`, `after`, and `on`

Before importing legacy native rules into Light-Fabric, the implementation
should define a small compatibility function registry for any gaps and convert
the rule to CEL. Candidate pure helper functions include:

```cel
contains_ignore_case(value, substring)
matches(value, pattern)
in_list(value, values)
contains_any(value, values)
contains_all(value, values)
```

These functions must be deterministic, side-effect free, and shared by the rule
tester and runtime evaluator. If Java parity is required, the same function names
and edge-case behavior should be implemented in the Java runtime.

## Safety

CEL support should be deterministic and sandboxed.

The evaluator does not need an operating-system sandbox for normal
trusted/admin-authored rule configuration. CEL is an interpreted expression
language, not arbitrary Rust or JavaScript execution, and expressions can only
resolve variables and functions registered in the CEL context. The CEL context
is therefore the primary sandbox boundary.

For the Rust `cel-interpreter` integration, context construction should be
explicit. `Context::default()` exposes standard pure CEL functions such as
`size`, `contains`, string helpers, type conversions, regex `matches`, and time
parsing helpers depending on enabled crate features. If a service accepts
tenant-authored or otherwise untrusted CEL, prefer `Context::empty()` and add
only platform-approved helper functions.

Security policy should be engine-owned. A rule may request a named condition
security profile, but it must not define its own function allowlist, size
limits, resource limits, or isolation mode. If a rule author controls the rule
body, then inline security settings are also attacker-controlled.

Recommended policy model:

```text
runtime config defines profiles:
  strict
  standard
  internal-admin

rule optionally requests:
  conditionSecurityProfile: strict

effective policy:
  runtime maximum profile intersected with requested profile
```

If a rule omits `conditionSecurityProfile`, the runtime default applies. If a
rule requests a profile that the service, tenant, or rule phase does not allow,
the rule config should be rejected during validation or runtime reload. The
engine may choose a stricter profile than requested, but it must never choose a
weaker one because the rule requested it.

Recommended profiles:

- `strict`: default for tenant-authored, portal self-service, imported, or
  marketplace-style CEL. Use an empty CEL context, expose only approved
  variables, add only pure helper functions, and enforce tight size and
  expression-shape limits. Do not expose the full `context` root, and disable
  regex until both Java and Rust provide matching bounded or linear-time
  behavior.
- `standard`: default for internal business rules. Keep allowlists and resource
  limits, but permit common pure helpers such as `size`, `contains`,
  `startsWith`, `endsWith`, `contains_ignore_case`, and bounded regex support
  if needed.
- `internal-admin`: limited to trusted operator-maintained rules. This may be
  closer to the selected CEL runtime's default behavior, but should still
  compile during rule load, validate references, enforce maximum input size, and
  protect reloads with the last known-good rule set.

Allowed:

- boolean logic
- comparisons
- arithmetic supported by the CEL implementation
- string operations
- list and map predicates
- approved pure helper functions

Not allowed:

- file access
- network access
- database access
- current time unless explicitly added as an input field
- random values
- mutation of the rule context
- action execution from inside CEL
- response-body mutation or field removal from CEL

Custom functions should be added conservatively. Standard Light-Rule actions
remain the extension point for side effects and transformations.

The core runtime object should be a policy-driven condition evaluator rather
than ad hoc logic embedded directly in `RuleEngine`:

```text
RuleEngineOptions
  -> ConditionExecutionPolicy
      -> defaultCelProfile
      -> allowRuleProfileSelection
      -> profiles[name] = CelSecurityProfile

CelSecurityProfile
  -> allowedFunctions
  -> allowedRootVariables
  -> exposeContextRoot
  -> exposeTopLevelAliases
  -> maxExpressionBytes
  -> maxContextBytes
  -> maxStringBytes
  -> maxCollectionItems
  -> allowRegex
  -> allowTimeParsing
  -> allowComprehensions
  -> maxComprehensionNesting
```

CEL still needs resource and robustness controls because expressions run on
request paths and can iterate over input data. Runtime and publish-time
validation should:

- allow-list functions and variables, using compiled expression references where
  available
- reject functions that perform I/O, mutation, service lookup, action execution,
  random generation, or implicit current-time access
- cap expression length and input context size
- reject or limit expensive access to large request or response bodies
- compile during rule load and fail invalid expression shapes before request
  execution
- keep the last known-good rule set if reload validation fails

Phase ceilings should be enforced by runtime policy. Response phases such as
`res-tra` and `res-fil` should default to a `strict` ceiling or tight
`maxContextBytes` limits because they can include large response payloads.
Access-control phases may allow `standard` only when the exposed context is
small and bounded. A rule request for a stronger profile than the phase ceiling
must be rejected or downgraded to the stricter effective profile.

For fully untrusted public input, evaluate CEL in a separate worker, process, or
another resource-isolated execution path with CPU and memory limits. A Tokio
timeout alone is not a complete guard for synchronous CPU-bound expression
evaluation.

## Portal Experience

Light-Portal should use `conditionLanguage` to choose the rule editor. For
Light-Fabric, the only supported editor is the CEL editor. Any native condition
builder must be scoped to legacy Java yaml-rule authoring and must not export
native condition rows to Light-Fabric runtime config.

Recommended authoring modes:

- `CEL`: advanced text area for one rule-level CEL expression.
- `Builder`: legacy Java yaml-rule-only condition rows with operand, operator,
  expected, and join controls.

Recommended behavior:

- default new Light-Fabric rules to `cel`
- render a CEL expression text area only for `conditionLanguage: cel`
- reject `conditionLanguage: native` for Light-Fabric rule publishing
- require confirmation when switching modes if the existing mode has content
- do not try to round-trip arbitrary CEL into native builder rows
- store the selected mode in `rule_t.condition_language` and in the JSON rule
  body as `conditionLanguage`
- for CEL rules, store only the selected profile name in
  `rule_t.condition_security_profile` and in the JSON rule body as
  `conditionSecurityProfile`; do not expose raw policy limits in the form
- do not show `internal-admin` in standard self-service forms; allow it only
  through checked-in runtime configuration or an explicitly authorized internal
  admin JWT/role path

The CEL editor should provide:

- syntax validation
- test context input
- expression result preview
- visible context field reference
- selected and effective security profile display
- rule test execution against the same backend evaluator used by runtime

## Profile Approval Workflow

Light-Portal may allow a user to select a CEL security profile, but the selected
profile is only a request. Runtime policy still computes the effective profile
from the requested profile, the service maximum, the tenant maximum, and the rule
phase ceiling.

Recommended publish behavior:

- `strict`: direct publish. If schema, CEL validation, and command authorization
  pass, create or update the rule immediately.
- `standard`: approval required. Submit the proposed rule change, create a
  worklist task for `rule-admin` and `admin`, and keep the change pending until
  approval.
- `internal-admin`: hidden from standard self-service authoring. If exposed to an
  operator-only flow, require stronger approval and never allow ordinary
  self-service users to request it.

For approval-required changes, the command side should not emit the final active
`RuleCreated` or `RuleUpdated` event at submission time. It should emit a
submission event such as `RuleChangeSubmittedEvent` or
`RuleApprovalRequestedEvent`, store the proposed rule body and requested profile,
and create the human-in-the-loop worklist task. Only approval should emit the
active rule event. Rejection should emit a rejection event and leave the active
rule unchanged.

Assistant tasks can help the approver by summarizing the CEL expression, rule
phase, requested profile, referenced context roots, use of response body fields,
regex usage, and any runtime ceiling that would downgrade the effective profile.
The assistant output is advisory only; the human approver remains responsible for
the approval decision.

Recommended approval rules:

- changing the expression, action list, rule phase, requested profile, or exposed
  context assumptions invalidates prior approval
- downgrading from `standard` to `strict` can publish directly after validation
- upgrading from `strict` to `standard` or `internal-admin` requires approval
- requester and approver should be different users except for an explicit
  break-glass workflow
- approval audit should record requested profile, effective profile, requester,
  approver, approval time, assistant-task summary id, and approval comments
- pending rules must not be exported to runtime endpoint rule config until
  approved

## Compatibility

Existing Light-Fabric rule YAML must use CEL conditions.

Rules without `conditionLanguage` can be treated as `cel` only when a valid
`expression` is present. Rules containing legacy native `conditions` must be
rejected by Light-Fabric runtime config validation or converted to CEL before
publish. The database migration should add `rule_t.condition_language` with
default `cel`.

Rules without `conditionSecurityProfile` use the runtime default CEL profile.
The field is meaningful only for CEL rules.

Native condition aliases are legacy Java yaml-rule import details only:

- `operatorCode` as alias for `operator`
- `propertyPath` as alias for `operand`
- `actionClassName` as alias for `actionRef`

CEL is the Light-Fabric capability. If the Java yaml-rule runtime needs to
execute the same rules, it must implement the same CEL rule shape. Until then,
Java runtimes must fail closed with a clear capability error, such as
`UnsupportedConditionLanguageException`, when loading or executing a rule with
`conditionLanguage: cel`. A runtime must not silently ignore a CEL rule because
that can fail open for access-control rules.

Java parity is feasible because Google maintains CEL-Java under the `dev.cel`
Maven group, including the `dev.cel:cel` artifact with compiler and runtime
APIs. The compatibility requirement is therefore mostly about aligning the rule
schema, context shape, custom functions, and error handling across the Rust and
Java runtimes.

## Example: Access Control

```yaml
ruleBodies:
  allowEndpointClaims:
    common: Y
    ruleId: allowEndpointClaims
    ruleName: Allow request when endpoint permission matches JWT claims
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      (
        !("role" in permission)
        || (
          ("roles" in auditInfo.subject_claims.ClaimsMap
            && permission.role in auditInfo.subject_claims.ClaimsMap.roles)
          || ("role" in auditInfo.subject_claims.ClaimsMap
            && permission.role == auditInfo.subject_claims.ClaimsMap.role)
        )
      )
      && (
        !("group" in permission)
        || (
          ("groups" in auditInfo.subject_claims.ClaimsMap
            && permission.group in auditInfo.subject_claims.ClaimsMap.groups)
          || ("scp" in auditInfo.subject_claims.ClaimsMap
            && permission.group in auditInfo.subject_claims.ClaimsMap.scp)
          || ("group" in auditInfo.subject_claims.ClaimsMap
            && permission.group == auditInfo.subject_claims.ClaimsMap.group)
          || ("grp" in auditInfo.subject_claims.ClaimsMap
            && permission.group == auditInfo.subject_claims.ClaimsMap.grp)
        )
      )
      && (
        !("position" in permission)
        || (
          ("positions" in auditInfo.subject_claims.ClaimsMap
            && permission.position in auditInfo.subject_claims.ClaimsMap.positions)
          || ("position" in auditInfo.subject_claims.ClaimsMap
            && permission.position == auditInfo.subject_claims.ClaimsMap.position)
          || ("pos" in auditInfo.subject_claims.ClaimsMap
            && permission.position == auditInfo.subject_claims.ClaimsMap.pos)
        )
      )
      && (
        !("attribute" in permission)
        || (
          ("attributes" in auditInfo.subject_claims.ClaimsMap
            && permission.attribute.key
              in auditInfo.subject_claims.ClaimsMap.attributes
            && auditInfo.subject_claims.ClaimsMap.attributes[
              permission.attribute.key
            ] == permission.attribute.value)
          || ("attribute" in auditInfo.subject_claims.ClaimsMap
            && permission.attribute.value
              == auditInfo.subject_claims.ClaimsMap.attribute)
          || ("att" in auditInfo.subject_claims.ClaimsMap
            && permission.attribute.value == auditInfo.subject_claims.ClaimsMap.att)
        )
      )
    actions: []

endpointRules:
  /v1/claims/{claimId}@post:
    req-acc:
      - allowEndpointClaims
    permission:
      role: claims-approver
      group: claims.write
      position: adjuster
      attribute:
        key: region
        value: east
```

The endpoint above matches a caller JWT with claims like:

```json
{
  "roles": ["claims-approver"],
  "groups": ["claims.write"],
  "positions": ["adjuster"],
  "attributes": {
    "region": "east"
  }
}
```

With this reusable `req-acc` rule, the technical rule body stays stable and API
owners define the required authorization dimensions at the endpoint. The example
above allows the request only when all configured endpoint permissions match
claims from the caller JWT.

## Example: Response Filter Guard

```yaml
ruleBodies:
  filterAccountsForPortalUsers:
    common: Y
    ruleId: filterAccountsForPortalUsers
    ruleName: Filter accounts for portal users
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200
      && responseBody != ""
      && auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
```

## Rollout Plan

1. Add `rule_t.condition_language` with default `cel`, optional
   `rule_t.condition_security_profile`, and check constraints.
2. Extend the rule specification with CEL rule validation plus optional
   `conditionSecurityProfile`, and reject native condition rows for
   Light-Fabric runtime config.
3. Add `conditionLanguage`, `conditionSecurityProfile`, and `expression` fields
   to the Rust `Rule` model.
4. Update command/query APIs so the portal can persist and read the condition
   language, security profile, and approval state without parsing `ruleBody`.
5. Reject `operatorCode: cel` in runtime config and normalize any legacy import
   to the rule-level CEL shape before publishing.
6. Choose and pin the Rust CEL crate behind an internal evaluator abstraction.
7. Add runtime-owned CEL security profiles and policy-driven context building.
8. Add approval workflow integration for `standard` and `internal-admin`
   profile requests, including worklist and assistant-task support.
9. Dispatch inside `RuleEngine::execute_rule` based on `conditionLanguage`.
10. Compile and cache CEL expressions during rule config load.
11. Add unit tests for CEL true, CEL false, invalid expression, mode validation,
   and missing-field behavior.
12. Add tests for custom legacy-operator compatibility helper functions.
13. Add performance tests for context conversion with large `toolArguments` and
   response payloads.
14. Add gateway integration tests using the existing rule context and the
   `context` root variable.
15. Add rule test API support so Light-Portal can validate CEL before publish.
16. Add CEL rule editing, a controlled CEL profile selector, and approval UX for
   stronger profile requests.
17. Document runtime compatibility and Java parity requirements.

## Decision

Support CEL conditions as the only Light-Fabric rule condition language. Native
condition rows remain a legacy Java yaml-rule format and must not be emitted to
Light-Fabric runtime configuration. A Light-Fabric rule should use
`conditionLanguage: cel`; mixed native/CEL condition arrays are not a supported
authoring or runtime model.
