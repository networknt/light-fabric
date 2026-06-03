# CEL Rule Conditions

Light-Rule should support both the existing native condition schema and CEL
expressions. The two forms solve different problems and should share the same
rule lifecycle, endpoint mapping, action execution, config loading, testing, and
governance model.

The native condition schema remains the default because it is easy to render in
Light-Portal, simple to validate, and suitable for most API-owner use cases. CEL
is an advanced condition form for customers that need richer boolean logic,
grouping, list predicates, or compatibility with existing CEL-based policy
assets.

Each rule should choose one condition language: `native` or `cel`. Mixing native
condition rows and CEL expressions inside the same rule is not recommended as the
canonical model because it makes portal authoring, validation, and runtime
dispatch harder to reason about.

## Goals

- keep existing rule YAML and portal-authored rules compatible
- support CEL expressions as a rule-level condition language
- evaluate native and CEL rules in the same `RuleEngine`
- reuse the existing rule context for gateway, workflow, and test execution
- preserve existing `actions`, `endpointRules`, and rule phase semantics
- let Light-Portal choose the correct editor from rule metadata without parsing
  arbitrary rule bodies
- validate CEL before publishing or reloading rules where possible
- keep CEL execution deterministic and side-effect free

## Non-Goals

- replacing the native Light-Rule condition schema
- replacing actions with CEL
- allowing CEL expressions to perform I/O, network calls, mutation, or service
  lookups
- making every native operator available as a custom CEL function on day one
- requiring business users to write CEL for common rules
- supporting mixed native and CEL condition blocks in the canonical portal
  authoring flow

## Current Model

Today a rule contains an optional flat list of native conditions:

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

Each native condition contains:

- `operator`
- `operand`
- `expected`
- `joinCode`

The engine evaluates conditions left-to-right. `joinCode` combines each
condition with the accumulated result. If the final condition result is true,
actions run as they do today.

Portal persistence stores rule metadata in `rule_t` and the executable rule JSON
in `rule_t.rule_body`. Today there is no dedicated column that tells the portal
which condition editor to render, so the UI would have to inspect `rule_body`.

## Proposed Rule Shape

Add a rule-level condition language flag. Use `native` for existing condition
rows and `cel` for a single CEL expression.

Persist the flag in both places:

- `rule_t.condition_language`: indexed/listable portal metadata
- `ruleBody.conditionLanguage`: self-contained exported runtime configuration

Recommended values:

```text
native
cel
```

Existing rules without the field are interpreted as `native`.

Native rule body:

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
ADD COLUMN condition_language VARCHAR(16) DEFAULT 'native' NOT NULL;

ALTER TABLE rule_t
ADD COLUMN condition_security_profile VARCHAR(32);

ALTER TABLE rule_t
ADD CONSTRAINT rule_t_condition_language_check
CHECK (condition_language IN ('native', 'cel'));

ALTER TABLE rule_t
ADD CONSTRAINT rule_t_condition_security_profile_check
CHECK (
  condition_security_profile IS NULL
  OR condition_security_profile IN ('strict', 'standard', 'internal-admin')
);
```

Recommended schema rules:

- `conditionLanguage` is optional and defaults to `native`
- `conditionLanguage: native` allows `conditions` and rejects `expression`
- `conditionLanguage: cel` requires `expression` and rejects `conditions`
- `conditionSecurityProfile` is optional and names a runtime-defined profile
- native conditions continue to require `operator` or `operatorCode`
- native conditions continue to require `operand` or `propertyPath`
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
      not:
        required: [expression]
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
| `rule-specification` | Add `conditionLanguage`, `conditionSecurityProfile`, `expression`, native rule and CEL rule schema branches, and mode/profile-specific validation rules. |
| `portal-db` | Add `rule_t.condition_language` with default `native`, optional `rule_t.condition_security_profile`, check constraints, and pending rule-change approval state if workflow task payloads are not sufficient. Keep existing rows valid without rewriting `rule_body`. |
| `light-portal` | Update persistence and projection code so rule create/update/read/export/import paths carry `conditionLanguage` and `conditionSecurityProfile`; ensure endpoint rule config generation emits only approved, self-contained rule bodies; integrate stronger-profile requests with worklist and assistant-task approval. |
| `rule-command` | Accept `conditionLanguage`, `conditionSecurityProfile`, and `expression`, normalize old/native payloads, validate mode/profile-specific shape, publish `strict` changes immediately, route stronger profile requests through approval, and write both DB metadata and rule body consistently after approval. |
| `rule-query` | Return `conditionLanguage`, `conditionSecurityProfile`, and approval status for list/detail APIs, include selected/effective profiles in test-case execution payloads, and surface CEL parse/type/missing-field/profile errors from Java and Rust runners. |
| `portal-view` | Render either the native condition builder or a CEL expression editor based on `conditionLanguage`; show a controlled profile selector for CEL rules; submit `strict` directly and route `standard` or `internal-admin` to worklist approval; do not require the UI to infer mode from `ruleBody`. |
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

This has one advantage: it can be implemented with a small Rust model change
because `operator`, `operand`, and `expected` already exist. It is useful as a
compatibility alias or import format.

It should not be the canonical schema because:

- CEL is a full boolean expression, not a comparison operator
- overloading `expected` makes validation and portal rendering less clear
- `operand` becomes ignored or artificial
- the UI still has to draw a condition-row editor even though the rule is really
  a single expression
- the rule schema still needs to change because the operator enum must include
  `cel` and native `operand` requirements must be relaxed
- future expression languages would continue overloading native condition fields

The recommended contract is therefore:

- canonical form: `conditionLanguage: cel` plus rule-level `expression`
- optional compatibility form: `operatorCode: cel` plus string `expected`
- normalize compatibility imports to the canonical rule-level model before
  persistence or runtime evaluation

## Mixed Conditions Alternative

Another possible shape is to allow native and CEL conditions in the same
`conditions` array. The runtime can support this if needed, but it should not be
the default authoring model.

Reasons to avoid canonical mixed rules:

- Light-Portal would need a hybrid editor that switches row-by-row
- validation errors become harder to explain to non-technical users
- `joinCode` semantics across native and CEL expressions are correct but subtle
- users may expect CEL operator precedence inside the whole rule even though
  native `joinCode` remains left-to-right
- runtime dispatch is simpler and faster when the rule selects one evaluator

If mixed rules are ever accepted for import or advanced API use, `joinCode`
should still apply left-to-right to the accumulated result regardless of which
evaluator handled the current or previous condition.

## Execution Model

Rule execution should dispatch by `conditionLanguage` once per rule:

```text
RuleEngine::execute_rule
  -> conditionLanguage == native
     -> evaluate native conditions
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

Runtime should treat a missing `conditionLanguage` as `native` for backward
compatibility.

## Rule Context

CEL should evaluate against the same JSON context used by native conditions.
For gateway access-control and response filtering, this includes fields such as:

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
require that shape to preserve compatibility with existing native rules and
actions.

For `standard` and `internal-admin` profiles, the CEL environment can expose
variables in two ways:

- top-level context fields as direct CEL variables, such as `auditInfo`,
  `toolArguments`, and `roles`
- the full root object as `context`, so expressions can use explicit paths such
  as `context.toolArguments.amount`

Direct variables keep expressions concise and close to the native condition path
style. The `context` variable is safer for generated expressions, collision
avoidance, and future fields that are not valid CEL identifiers.

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

Validation must enforce mode-specific shape:

- `native`: `conditions` is allowed, `expression` is rejected
- `cel`: `expression` is required, `conditions` is rejected
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

## Native Operator Parity

The native evaluator includes operators that may not map one-to-one to the
selected CEL runtime. Examples include:

- `containsIgnoreCase`
- `matches` and `notMatch`
- `inList` and `notInList`
- `containsAny`, `containsAll`, and `containsNone`
- date-style comparisons such as `before`, `after`, and `on`

Before encouraging migration from native conditions to CEL, the implementation
should define a small compatibility function registry for any gaps. Candidate
pure helper functions include:

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

Custom functions should be added conservatively. Native Light-Rule actions
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

Light-Portal should use `conditionLanguage` to choose the rule editor. This
keeps the form predictable and avoids mixing two mental models on the same
screen.

Recommended authoring modes:

- `Builder`: native condition rows with operand, operator, expected, and join
  controls
- `CEL`: advanced text area for one rule-level CEL expression

Recommended behavior:

- default new rules to `native`
- render condition subforms only for `conditionLanguage: native`
- render a CEL expression text area only for `conditionLanguage: cel`
- hide native condition controls when CEL is selected
- hide CEL expression controls when native is selected
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

Existing rule YAML remains valid.

Rules without `conditionLanguage` are treated as `native`. The database
migration should add `rule_t.condition_language` with default `native`, so
existing rows do not need their `rule_body` rewritten immediately.

Rules without `conditionSecurityProfile` use the runtime default CEL profile.
The field is meaningful only for CEL rules; native rules do not need a condition
security profile.

Native condition aliases must continue to work:

- `operatorCode` as alias for `operator`
- `propertyPath` as alias for `operand`
- `actionClassName` as alias for `actionRef`

CEL introduces a new capability. If the Java yaml-rule runtime needs to execute
the same rules, it must implement the same CEL rule shape. Until then, Java
runtimes must fail closed with a clear capability error, such as
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
  allowApprovedTransfer:
    common: Y
    ruleId: allowApprovedTransfer
    ruleName: Allow approved transfer
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      auditInfo.subject_claims.ClaimsMap.role in roles
      && (
        toolArguments.amount < 1000
        || "transfer.approve" in auditInfo.subject_claims.ClaimsMap.scope
      )
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction

endpointRules:
  /transfer@post:
    req-acc:
      - allowApprovedTransfer
    permission:
      roles:
        - teller
        - approver
```

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

1. Add `rule_t.condition_language` with default `native`, optional
   `rule_t.condition_security_profile`, and check constraints.
2. Extend the rule specification with native and CEL rule branches plus
   optional `conditionSecurityProfile`.
3. Add `conditionLanguage`, `conditionSecurityProfile`, and `expression` fields
   to the Rust `Rule` model.
4. Update command/query APIs so the portal can persist and read the condition
   language, security profile, and approval state without parsing `ruleBody`.
5. Optionally accept `operatorCode: cel` as an import/compatibility alias and
   normalize it to the rule-level CEL shape.
6. Choose and pin the Rust CEL crate behind an internal evaluator abstraction.
7. Add runtime-owned CEL security profiles and policy-driven context building.
8. Add approval workflow integration for `standard` and `internal-admin`
   profile requests, including worklist and assistant-task support.
9. Dispatch inside `RuleEngine::execute_rule` based on `conditionLanguage`.
10. Compile and cache CEL expressions during rule config load.
11. Add unit tests for CEL true, CEL false, invalid expression, mode validation,
   and missing-field behavior.
12. Add tests for custom native-parity helper functions.
13. Add performance tests for context conversion with large `toolArguments` and
   response payloads.
14. Add gateway integration tests using the existing rule context and the
   `context` root variable.
15. Add rule test API support so Light-Portal can validate CEL before publish.
16. Add portal mode-based rule editing, a controlled CEL profile selector, and
   approval UX for stronger profile requests.
17. Document runtime compatibility and Java parity requirements.

## Decision

Support both condition languages. Native Light-Rule conditions remain the
stable, portal-friendly default. CEL becomes an optional advanced expression
language inside the same rule engine for customers that need richer policy
expressions. A rule should select one condition language through
`conditionLanguage`; mixed native/CEL condition arrays are not the canonical
authoring model.
