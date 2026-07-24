# Debugging CEL Rules

## Status

The short-term referenced-context trace logging described here is implemented.
Structured decision outcomes and the rule-test API remain proposed long-term
work.

## Problem

The Light-Gateway access-control handler and MCP router both evaluate CEL rules
through the shared access-control runtime. When a request is denied, a rule
author usually sees only a generic access-denied response. That response does
not distinguish among these cases:

- the CEL expression evaluated successfully and returned `false`
- CEL compilation or evaluation failed
- the expression returned a non-boolean value
- the endpoint referenced a missing rule
- an action rejected the rule after its CEL condition matched
- `accessRuleLogic: all` or `accessRuleLogic: any` produced the final denial
- `defaultDeny` applied because no endpoint or `req-acc` rule matched
- MCP `tools/list` hid a tool because of policy, an unknown rule, or the
  `maxCelEvaluations` limit

These cases must remain fail-closed, but they should not be indistinguishable to
an authorized operator or rule author.

Printing the complete request on every denial is not an acceptable solution.
Access control runs after security, so its CEL context should contain normalized
identity claims and policy inputs rather than raw authentication credentials.
Diagnostics can then project only the properties referenced by the expression
instead of copying unrelated claims, headers, tool arguments, request data, or
response data.

## Current Behavior

The current implementation already provides a useful starting point:

- `RuleEngine` catches CEL execution errors and interpreter panics.
- Failed CEL evaluations and `false` results can emit a separate `TRACE` event
  containing only statically referenced context properties. Metadata is logged
  by default; `logFullCelContext: true` logs bounded values.
- Context diagnostics bound depth, node count, collection size, string length,
  key length, and null-path traversal.
- Access-control context construction excludes authorization, proxy
  authorization, cookie, set-cookie, and API-key headers.
- Successful CEL evaluation returns only a boolean.
- The shared access-control runtime converts a missing rule or any rule-engine
  error into `false` for request authorization.
- The final HTTP and MCP denial responses deliberately avoid exposing internal
  policy details.

The main gap is therefore not only context visibility. It is loss of structured
decision information between CEL execution and the final access decision.

## Goals

- Explain why an HTTP request, MCP tool call, or MCP tool-list entry was allowed,
  denied, hidden, or filtered.
- Distinguish a valid `false` result from a malformed rule or runtime error.
- Show the effective CEL context during local and development rule testing.
- Use the exact runtime evaluator for pre-deployment rule testing.
- Correlate diagnostics with the request, policy revision, endpoint, tool, and
  rule version that produced the decision.
- Preserve current fail-closed authorization behavior.
- Keep diagnostic overhead negligible when debugging is disabled.
- Share the implementation between HTTP access control and MCP routing.
- Keep raw credentials and other security-handler inputs out of the CEL context.

## Non-Goals

- Return policy internals or request context to ordinary API or MCP callers.
- Log complete request, response, token, or tool-argument payloads by default.
- Rewrite CEL expressions into simpler expressions for diagnostic evaluation.
  Rewriting could change short-circuiting, macros, presence behavior, or types.
- Turn CEL into a mutation or general scripting language.
- Guarantee a natural-language proof for every `false` result. The first
  implementation should report facts and outcomes rather than speculate.
- Weaken `strict` profile field exposure to make debugging easier.
- Support CEL rules that inspect raw authorization headers, cookies, API keys,
  tokens, or other authentication credentials.

## Design Principles

### Separate enforcement from explanation

Authorization still maps every non-matching or erroneous outcome to deny when
the policy requires fail-closed behavior. A separate diagnostic result retains
the reason for authorized consumers.

### Prefer pre-deployment testing

The best production diagnostic is a rule that was tested before publication.
Runtime diagnostics remain necessary because live tokens, headers, endpoint
resolution, and tool arguments can differ from test fixtures.

### Report facts, not invented explanations

For a `false` result, report the expression, statically referenced paths,
structural metadata or full values according to `logFullCelContext`, profile,
and rule-combination behavior. Do not claim that a particular clause caused the
result because the current CEL evaluator has no execution observer that can
prove it.

## Outcome Model

The boolean returned by the current rule path should be replaced internally by
a structured outcome. The exact Rust types can evolve, but the semantic model
should be stable:

```rust
enum RuleConditionOutcome {
    Matched,
    NotMatched,
    CompileError { message: String, source: Option<SourceLocation> },
    EvaluationError { message: String },
    NonBoolean { actual_type: String },
    SecurityProfileRejected { message: String },
}

enum RuleExecutionOutcome {
    Matched,
    ConditionNotMatched,
    ConditionError(RuleConditionOutcome),
    ActionRejected { action_ref: String },
    ActionError { action_ref: String, message: String },
    ActionNotFound { action_ref: String },
    RuleNotFound,
}
```

`RuleEngine::execute_rule` should return a structured result. Compatibility
wrappers can continue returning `bool` where callers do not need diagnostics.

The shared access-control runtime should then build an aggregate decision:

```rust
struct AccessEvaluation {
    decision: AccessDecision,
    reason: AccessDecisionReason,
    rules: Vec<RuleEvaluation>,
    skipped_rule_ids: Vec<String>,
}
```

`AccessDecision` remains the enforcement projection. `AccessEvaluation` is the
diagnostic projection.

## Rule Aggregation Trace

The trace must preserve `accessRuleLogic` behavior:

- With `all`, evaluation stops at the first rule that does not match or errors.
  Remaining rule IDs are recorded as skipped because of short-circuiting.
- With `any`, evaluation stops at the first matching rule. Remaining rule IDs
  are recorded as skipped because of short-circuiting.
- A rule-engine error is recorded as an error outcome even when the enforcement
  projection treats it like `false`.
- A missing rule body is recorded as `rule_not_found`, not `not_matched`.
- `defaultDeny` decisions are recorded without fabricating a rule evaluation.

Actions can mutate the rule context, so the existing candidate-context behavior
for `any` must remain unchanged. Diagnostic collection must observe the same
execution and must not evaluate a rule a second time.

## Decision Trace

A decision trace should use a stable structured shape suitable for JSON logs and
the future rule-test API:

```json
{
  "timestamp": "2026-07-24T15:42:11.184Z",
  "correlationId": "request-123",
  "serviceId": "com.networknt.gateway-1.0.0",
  "policyRevision": "sha256...",
  "surface": "mcp-tools-call",
  "endpoint": "/config/query@post",
  "toolName": "queryConfig",
  "ruleType": "req-acc",
  "ruleLogic": "all",
  "decision": "denied",
  "reason": "condition_not_matched",
  "rules": [
    {
      "ruleId": "allow-config-read",
      "expressionHash": "sha256...",
      "requestedProfile": "strict",
      "effectiveProfile": "strict",
      "outcome": "condition_not_matched",
      "contextMode": "full",
      "referencedPaths": [
        "permission.roles",
        "auditInfo.subject_claims.ClaimsMap.roles"
      ],
      "referencedValues": {
        "permission.roles": ["config-admin"],
        "auditInfo.subject_claims.ClaimsMap.roles": ["developer"]
      },
      "contextTruncated": false
    }
  ],
  "skippedRuleIds": [],
  "referenceAnalysisIncomplete": false,
  "traceTruncated": false
}
```

Trace logs can include the expression text because this feature is intended for
local and development use. They should also include `ruleId`, expression hash,
and policy revision so that a diagnostic can be tied to the exact loaded policy.

## Diagnostic Context Modes

The runtime has two context projections:

| Mode | Contents | Intended Use |
| --- | --- | --- |
| metadata | referenced paths plus presence, JSON type, null state, and collection or string size without property values | default trace behavior |
| full | actual values for statically referenced CEL properties | local and development environments only |

Full context must never mean an unbounded raw dump. Both modes use the same
CEL-profile projection and diagnostic budgets as evaluator-error diagnostics.

## CEL Reference Discovery

`light-rule` currently pins `cel 0.14.0`. That crate exposes two relevant APIs:

- `Program::references()` returns the root variables and functions referenced
  by the compiled expression. For `auditInfo.subject_claims.ClaimsMap.roles`,
  it reports the root variable `auditInfo`.
- `Program::expression()` exposes the public parsed AST. `Expr::Select` nodes
  contain their operand and selected field, so Light-Fabric can walk the AST and
  recover the complete static member path
  `auditInfo.subject_claims.ClaimsMap.roles`.

The crate does not expose an evaluation observer or a list of properties
actually read at runtime. Reference discovery is therefore static: it includes
properties in branches that short-circuit evaluation and cannot always resolve
computed map keys.

The compiled-program cache should store a reference projection alongside each
program:

```rust
struct CelProgramEntry {
    program: Arc<CelProgram>,
    referenced_roots: Vec<String>,
    referenced_paths: Vec<String>,
    reference_analysis_incomplete: bool,
}
```

Static dot selections and indexes with literal string keys should produce exact
paths. For dynamic indexing, the projection should fall back to the smallest
known root or static prefix and set `referenceAnalysisIncomplete: true`. Macro
and comprehension-local variables must not be mistaken for root context
variables.

This fallback deliberately broadens full mode. For example,
`ClaimsMap[claimName]` cannot identify the selected claim statically, so full
mode emits the bounded `ClaimsMap` parent object as well as `claimName`.
Metadata mode emits only the parent's type and size. Rule authors should prefer
literal indexes or dot selections when they want the narrowest diagnostic
projection.

The reference walker is coupled to the public AST and operator names in the
pinned `cel 0.14.0` crate. A CEL dependency upgrade must revalidate the walker
and its literal-index, dynamic-index, and comprehension tests.

## Access-Control Context Boundary

Security authenticates the request before access control runs. The security
handler should expose normalized identity and authorization facts through
`auditInfo.subject_claims.ClaimsMap`; it should not forward the credential used
to establish those facts into CEL.

The access-control CEL context must therefore exclude raw values such as:

- `Authorization` and `Proxy-Authorization` headers
- cookies and session tokens
- API keys and client secrets
- private keys or credential material owned by an earlier handler

If a rule needs an identity fact derived from one of these inputs, the security
handler should expose the normalized claim instead. For example, a rule should
read a roles claim from `auditInfo`, not parse the bearer token.

Other CEL inputs should be policy-oriented: endpoint and tool identity,
permissions, selected non-sensitive headers, correlation metadata, referenced
tool arguments, and the request or response properties required by the rule
phase.

## Referenced Context Projection

Diagnostics start from the variables exposed by the effective CEL security
profile and keep only the statically referenced properties. A diagnostic cannot
include an unrelated property merely because it exists in the root context.

The projection then keeps only the statically referenced properties. Most
current request-access rules reference JWT claims below
`auditInfo.subject_claims.ClaimsMap`, so a rule that reads only the caller's
roles should not cause unrelated headers, claims, or tool arguments to be
logged.

The diagnostic path does not add header or JSON-path masking. Sensitive
credentials are excluded when the access-control context is constructed, and
reference projection removes unrelated properties. `logFullCelContext: true`
can therefore emit the actual values of referenced policy properties. It is
intentionally a local and development-only setting and must emit a startup
warning.

### Values that require special handling

- JWT claims and `toolArguments` include only statically referenced properties.
- `responseBody` and `responseBodyJson` include only statically referenced
  properties.
- A row-level CEL filter captures at most the current bounded row and should not
  repeat the shared context for every rejected row.
- Binary data is represented by type and length, not encoded into the trace.

### Bounds

Reuse the existing limits for diagnostic context depth, nodes, collection items,
string characters, key characters, null paths, and null-path traversal. Add an
overall serialized trace byte limit. Every limit must have a corresponding
truncation field so an operator can distinguish absent data from omitted data.

## Runtime Configuration

The short-term configuration should be one root-level property in
`access-control.yml`:

```yaml
logFullCelContext: false
```

This property does not enable trace logging. The logging filter still controls
whether CEL trace events are emitted, for example:

```text
RUST_LOG=light_rule::cel=trace,info
```

The property controls only the context projection used by those events:

- `false` or absent: emit referenced paths and structural metadata without
  property values at `TRACE`
- `true`: emit actual values for the referenced CEL properties at `TRACE` for
  local or development use

No mode performs diagnostic masking. The serialized trace remains size-limited
and reports truncation, but full-mode values are otherwise emitted as they
appear in the credential-free access-control context.

The runtime should emit a prominent startup warning when
`logFullCelContext: true` is loaded:

```text
Full CEL context logging is enabled. This setting is intended only for local or development environments.
```

Trace events should cover CEL evaluation errors and successful evaluations that
return `false`. A per-rule `false` event must be labeled as a rule outcome, not
as a final access denial, because `accessRuleLogic: any` can allow the request
through a later rule.

This changes earlier evaluator-error logging behavior: bounded context and
candidate-null-path diagnostics previously appeared with the `WARN` or `ERROR`
event. They now appear only in the separate `light_rule::cel` `TRACE` event;
the warning or error retains the expression and failure details without request
context. Operators who relied on warning-level context must enable the trace
target while diagnosing CEL failures.

## Rule-Test API

Rule authors should be able to test before publishing. The test path must use
the same `light-rule` evaluator, profile enforcement, context conversion, and
action registry as the target runtime.

Suggested request:

```json
{
  "rule": {
    "ruleId": "allow-config-read",
    "ruleType": "req-acc",
    "conditionLanguage": "cel",
    "conditionSecurityProfile": "strict",
    "expression": "permission.roles.exists(r, r in auditInfo.subject_claims.ClaimsMap.roles)"
  },
  "context": {
    "auditInfo": {
      "subject_claims": {
        "ClaimsMap": {
          "roles": ["developer"]
        }
      }
    },
    "permission": {
      "roles": ["config-admin"]
    },
    "endpoint": "/config/query@post",
    "toolName": "queryConfig",
    "toolArguments": {}
  }
}
```

Suggested response:

```json
{
  "outcome": "condition_not_matched",
  "requestedProfile": "strict",
  "effectiveProfile": "strict",
  "referencedPaths": [
    "permission.roles",
    "auditInfo.subject_claims.ClaimsMap.roles"
  ],
  "referencedValues": {
    "permission.roles": ["config-admin"],
    "auditInfo.subject_claims.ClaimsMap.roles": ["developer"]
  },
  "warnings": [],
  "contextTruncated": false
}
```

The API should support two modes:

- isolated rule evaluation for the editor
- endpoint-policy evaluation using a selected configuration snapshot, including
  rule ordering, `all` or `any`, permissions, and default-deny behavior

The endpoint-policy mode is essential because a rule can work in isolation but
still not be selected for the deployed endpoint.

The test API must not accept arbitrary production credentials or fetch live
requests. Test contexts are explicit input, access-controlled, size-limited,
and excluded from ordinary logs.

## Portal Experience

The CEL editor should present:

- the documented context schema for the selected rule phase
- requested and effective security profiles
- syntax and profile validation before publication
- an editable constructed test context
- matched, not-matched, or error as distinct states
- per-rule endpoint-policy results and short-circuiting
- referenced paths beside their constructed test values
- missing-field, null-receiver, type, and non-boolean errors

A production denial shown to an ordinary caller remains generic. Rule testing
uses a constructed context in the Portal rather than retrieving a live request
context from the gateway.

## MCP-Specific Behavior

The same trace model applies to MCP with a `surface` field that identifies:

- `mcp-tools-call`
- `mcp-tools-list`
- `mcp-response-filter`

For `tools/call`, include the resolved configured tool name and endpoint, but
include only tool-argument properties referenced by the CEL expression.

For CEL-based `tools/list`, one request can evaluate many tools. Do not emit one
large context event per hidden tool by default. Emit a bounded aggregate summary
with counts and keep any per-tool trace detail bounded. Distinguish:

- hidden by a CEL `false` result
- hidden by CEL error
- hidden by unknown-rule fallback
- skipped after `maxCelEvaluations`
- served from the tools-list visibility cache

If a result came from cache, include the policy revision and cache outcome. Do
not claim that CEL was evaluated for that request.

## Response-Filter Behavior

Response filtering needs separate outcomes because `false` can mean different
things at different layers:

- rule-level CEL condition did not select the filter
- a response-filter action rejected execution
- a row-level CEL expression excluded a row
- a row-level expression failed and the row was excluded fail-closed
- the complete top-level object was denied

Row filtering can evaluate the same expression hundreds or thousands of times.
Capture the first bounded failure sample, total matched/excluded/error counts,
and the number of suppressed diagnostics. Never emit the entire response body.

## Logs, Metrics, and Audit

Use stable tracing targets and event names rather than prose-only messages:

```text
target: light_rule::decision
event: cel_rule_decision
```

Summary events should contain scalar fields that remain useful in text and JSON
logging. Detailed context can be a bounded JSON field.

Recommended counters:

- CEL evaluations by outcome, rule type, and security profile
- access decisions by reason and surface
- diagnostic traces captured, truncated, sampled out, or rate-limited
- rule-test requests by outcome
- tools-list evaluations skipped by `maxCelEvaluations`

Do not use rule IDs, endpoints, tool names, correlation IDs, or user identities
as unbounded metric labels. Those belong in logs or traces.

Individual rule evaluations normally remain operational tracing events, not
durable security audit records, unless deployment policy requires otherwise.

## Performance and Abuse Controls

- When `TRACE` is disabled for the CEL tracing target, avoid cloning or
  serializing context solely for diagnostics.
- Build a context projection only after confirming that the trace event is
  enabled.
- Reuse the compiled CEL program and any compile-time reference analysis.
- Never reevaluate a CEL expression to explain its first result.
- Bound trace size, row samples, and tools-list samples.
- Rate-limit rule-test execution.
- Apply existing CEL profile and expression-complexity limits to test requests.
- Record when sampling or limits omitted diagnostic data.

## Failure Behavior

Diagnostics must never change the access decision. If reference analysis,
serialization, or log emission fails:

1. preserve the original allow or deny result
2. emit a bounded diagnostic-system error without request context
3. increment a diagnostic failure counter
4. do not retry on the request path

The diagnostic system itself should be panic-contained where it processes
untrusted context values.

## Implementation Phases

### Phase 1: Preserve outcomes

- Introduce structured condition and rule outcomes in `light-rule`.
- Stop collapsing every `RuleEngine` error into an unexplained `false` inside
  the shared access-control runtime.
- Preserve boolean compatibility wrappers for unrelated callers.
- Add aggregate outcomes for `all`, `any`, missing rules, and default deny.
- Emit safe summary tracing events for errors and denials.

### Phase 2: Referenced diagnostic projection

Implemented for the short-term runtime diagnostic path:

- Exclude raw credentials when constructing the access-control CEL context.
- Centralize context reference analysis, projection, and bounds.
- Apply the same safe projection to existing CEL error and panic logs.
- Use `Program::references()` and the public AST to extract referenced roots and
  static member paths.
- Add the root-level `logFullCelContext` configuration to `access-control.yml`.

Remaining enhancements:

- Add expression hashes, policy revisions, and stable reason codes.
- Add MCP tools-list aggregation and response-row sampling.

### Phase 3: Authoring workflow

- Add isolated-rule and endpoint-policy test APIs.
- Integrate the APIs into the Portal CEL editor.
- Validate rules with the target runtime evaluator before publication.

## Testing Strategy

### Outcome tests

- CEL `true` and `false`
- compile error, evaluation error, panic, and non-boolean result
- missing rule and missing action
- action rejection and action error
- `all` and `any` short-circuit traces
- default allow and default deny
- HTTP, MCP `tools/call`, MCP `tools/list`, and response-filter surfaces

### Security tests

- raw authorization headers, cookies, API keys, and credentials never enter the
  access-control CEL context
- strict diagnostics never expose roots unavailable to strict CEL
- unrelated headers, claims, tool arguments, and response fields are absent
- metadata mode reports structure without property values
- full mode reports values only for referenced properties
- request input cannot enable full context logging
- truncation flags are correct

### Parity tests

- rule-test and runtime execution return the same outcome for the same rule,
  context, profile, and policy revision
- diagnostic collection does not change action mutation or short-circuiting
- cached MCP tools-list decisions are labeled as cached and are not reported as
  fresh evaluations

### Performance tests

- disabled trace logging has no material request-path allocation regression
- metadata and full trace logging remain within defined latency budgets
- large contexts, large rows, and tools-list fan-out remain bounded

## Resolved Decisions

1. The pinned `cel 0.14.0` crate exposes referenced root variables and a public
   AST, so Light-Fabric will statically extract related context paths and log
   only those properties. It does not expose actual runtime property reads.
2. The diagnostic path will not add masking. Access-control context construction
   excludes raw credentials, and full mode logs the actual bounded values of
   referenced policy properties.
3. The design does not add log retention or audit requirements. Local logging
   and its existing rotation policy own trace retention.

## Recommended Default

The referenced-context trace logging and root-level `logFullCelContext` switch
are the short-term implementation. Next, add structured outcomes, then the
rule-test API and Portal editor support.

This sequence fixes the information-loss problem, gives authors a safe way to
inspect rule context during local development and test rules before deployment,
and can adopt runtime property-read tracing later if the CEL evaluator adds a
trustworthy observer API.
