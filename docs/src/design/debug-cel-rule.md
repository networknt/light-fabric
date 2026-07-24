# Debugging CEL Rules

## Status

This document proposes a debugging model for CEL rules executed by
Light-Fabric. The diagnostic configuration, structured outcomes, diagnostic
sessions, and rule-test API described below are not implemented unless a
section explicitly identifies current behavior.

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

Printing the complete CEL context on every denial is not an acceptable
solution. The context can contain authorization headers, cookies, JWT claims,
tool arguments, request data, and response data. An unconditional dump would
create credential-exposure, privacy, log-volume, and denial-of-service risks.

## Current Behavior

The current implementation already provides a useful starting point:

- `RuleEngine` catches CEL execution errors and interpreter panics.
- CEL errors log the expression, a bounded `evaluationContext`, candidate null
  paths, and truncation indicators.
- Context diagnostics bound depth, node count, collection size, string length,
  key length, and null-path traversal.
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
- Show the effective CEL context safely enough for production diagnostics.
- Use the exact runtime evaluator for pre-deployment rule testing.
- Correlate diagnostics with the request, policy revision, endpoint, tool, and
  rule version that produced the decision.
- Preserve current fail-closed authorization behavior.
- Keep diagnostic overhead negligible when debugging is disabled.
- Share the implementation between HTTP access control and MCP routing.

## Non-Goals

- Return policy internals or request context to ordinary API or MCP callers.
- Log complete request, response, token, or tool-argument payloads by default.
- Rewrite CEL expressions into simpler expressions for diagnostic evaluation.
  Rewriting could change short-circuiting, macros, presence behavior, or types.
- Turn CEL into a mutation or general scripting language.
- Guarantee a natural-language proof for every `false` result. The first
  implementation should report facts and outcomes rather than speculate.
- Weaken `strict` profile field exposure to make debugging easier.

## Design Principles

### Separate enforcement from explanation

Authorization still maps every non-matching or erroneous outcome to deny when
the policy requires fail-closed behavior. A separate diagnostic result retains
the reason for authorized consumers.

### Prefer pre-deployment testing

The best production diagnostic is a rule that was tested before publication.
Runtime diagnostics remain necessary because live tokens, headers, endpoint
resolution, and tool arguments can differ from test fixtures.

### Make production debugging temporary and targeted

Detailed tracing should be activated for a short time and selected by a trusted
server-side filter such as correlation ID, rule ID, endpoint, or tool name. An
untrusted client header must not enable diagnostic context capture.

### Report facts, not invented explanations

For a `false` result, report the expression, referenced paths, sanitized values,
profile, and rule-combination behavior. Do not claim that a particular clause
caused the result unless the CEL evaluator provides an execution observer that
can prove it.

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

A decision trace should use a stable structured shape suitable for JSON logs,
an operator API, and Portal rendering:

```json
{
  "diagnosticId": "01K...",
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
  "redactedPaths": [],
  "traceTruncated": false
}
```

The trace should include the expression text only in an authorized detail view.
Normal logs should prefer `ruleId`, expression hash, and policy revision so that
large expressions are not repeated and a diagnostic can be tied to the exact
loaded policy.

## Diagnostic Detail Levels

Three levels keep cost and exposure proportional to the debugging need:

| Level | Contents | Intended Use |
| --- | --- | --- |
| `summary` | decision, reason, rule IDs, outcomes, profile, correlation ID, policy revision | production metrics and routine logs |
| `referenced` | summary plus statically referenced CEL paths and sanitized values | targeted production debugging |
| `context` | referenced detail plus bounded sanitized exposed context | local development and tightly controlled operator sessions |

`context` must never mean an unbounded raw dump. It uses the same CEL-profile
projection and diagnostic budgets as evaluator-error diagnostics.

Static path extraction is best effort. Dynamic map indexing and computed keys
may prevent an exact list. The trace should set `referenceAnalysisIncomplete`
instead of silently implying that the list is complete.

## Safe Context Projection

Diagnostics should start from the variables actually exposed by the effective
CEL security profile. A diagnostic must not reveal a value that CEL itself was
not allowed to access.

The projection then applies redaction and bounds.

### Always-redacted headers

At minimum, redact these case-insensitive header names:

- `authorization`
- `proxy-authorization`
- `cookie`
- `set-cookie`
- `x-api-key`

Deployments should be able to add header names without replacing the built-in
list.

### Sensitive JSON paths

Support configured JSON-pointer or dotted-path masks for claims, tool arguments,
request data, rows, and response data. Built-in key-name protection should mask
common names such as `password`, `secret`, `token`, `apiKey`, `privateKey`, and
`credential`, case-insensitively. Explicit path configuration takes precedence
over heuristic masking.

### Values that require special handling

- JWT claims are visible only to an authorized operator and remain subject to
  configured masks.
- `toolArguments` defaults to referenced fields only.
- `responseBody` and `responseBodyJson` are excluded from production traces by
  default, even at `context` level.
- A row-level CEL filter captures at most the current bounded row and should not
  repeat the shared context for every rejected row.
- Binary data is represented by type and length, not encoded into the trace.

### Bounds

Reuse the existing limits for diagnostic context depth, nodes, collection items,
string characters, key characters, null paths, and null-path traversal. Add an
overall serialized trace byte limit. Every limit must have a corresponding
truncation field so an operator can distinguish absent data from omitted data.

## Runtime Configuration

The shared policy is loaded from `access-control.yml`, so the initial static
configuration should live there:

```yaml
ruleDiagnostics:
  enabled: false
  outcomeMode: errors       # errors | denied | all
  detailLevel: summary      # summary | referenced | context
  sampleRate: 1.0
  maxTraceBytes: 16384
  includeExpression: false
  includeResponseBody: false
  redactHeaders:
    - x-tenant-secret
  redactPaths:
    - toolArguments.password
    - auditInfo.subject_claims.ClaimsMap.email
```

Defaults must be safe:

- diagnostics disabled
- error summaries continue using the existing bounded evaluator diagnostics
- no response body
- no expression text in routine structured logs
- built-in redaction cannot be removed

Static `outcomeMode: denied` or `all` is useful in local development but is too
broad for normal production use.

## Diagnostic Sessions

Production detail should be enabled through a short-lived diagnostic session
managed by an authenticated operator surface. A session can select:

- service instance or service ID
- correlation ID
- endpoint
- MCP tool name
- rule ID
- outcome class
- detail level
- maximum captured decisions
- expiration time

Example request:

```json
{
  "filters": {
    "ruleId": "allow-config-read",
    "outcomes": ["condition_not_matched", "evaluation_error"]
  },
  "detailLevel": "referenced",
  "maxDecisions": 20,
  "expiresInSeconds": 300
}
```

The runtime should reject sessions without an expiration and enforce hard
maximums for duration and event count. Session creation, access, expiration,
and deletion must be audited. A client-supplied request header can be used as a
correlation value, but it must not create or broaden a diagnostic session.

Captured traces may be emitted directly as structured tracing events or kept in
a small bounded in-memory ring buffer exposed through the module-registry
management surface. A ring buffer makes retrieval reliable when normal logging
filters exclude debug events, but it must have strict memory, TTL, and access
controls.

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
- an editable test context or a sanitized captured-context import
- matched, not-matched, or error as distinct states
- per-rule endpoint-policy results and short-circuiting
- referenced paths beside their sanitized values
- missing-field, null-receiver, type, and non-boolean errors
- a copyable `diagnosticId` and correlation ID for production incidents

A production denial shown to an ordinary caller remains generic. An authorized
Portal user can look up the diagnostic separately, preventing accidental policy
disclosure through the application protocol.

## MCP-Specific Behavior

The same trace model applies to MCP with a `surface` field that identifies:

- `mcp-tools-call`
- `mcp-tools-list`
- `mcp-response-filter`

For `tools/call`, include the resolved configured tool name and endpoint, but
mask arguments according to tool metadata and diagnostic policy.

For CEL-based `tools/list`, one request can evaluate many tools. Do not emit one
large context event per hidden tool by default. Emit a bounded aggregate summary
with counts and allow per-tool detail only in a targeted session. Distinguish:

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

Diagnostic session administration is security-relevant and must produce audit
events. Individual rule evaluations normally remain operational tracing events,
not durable security audit records, unless deployment policy requires otherwise.

## Performance and Abuse Controls

- When diagnostics are disabled, avoid cloning or serializing context solely for
  tracing.
- Build a decision trace only after a configured outcome and session filter
  match.
- Reuse the compiled CEL program and any compile-time reference analysis.
- Never reevaluate a CEL expression to explain its first result.
- Bound diagnostic session count, trace count, trace size, ring-buffer memory,
  row samples, and tools-list samples.
- Rate-limit operator retrieval and rule-test execution.
- Apply existing CEL profile and expression-complexity limits to test requests.
- Record when sampling or limits omitted diagnostic data.

## Failure Behavior

Diagnostics must never change the access decision. If redaction, reference
analysis, serialization, storage, or log emission fails:

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

### Phase 2: Safe diagnostic projection

- Centralize context projection, redaction, and bounds.
- Apply the same safe projection to existing CEL error and panic logs.
- Add expression hashes, policy revisions, stable reason codes, and diagnostic
  IDs.
- Add best-effort CEL reference extraction.

### Phase 3: Targeted runtime sessions

- Add static `ruleDiagnostics` configuration.
- Add authenticated, expiring diagnostic sessions through the runtime
  management surface.
- Add the bounded trace buffer and retrieval contract if logs alone are
  insufficient.
- Add MCP tools-list aggregation and response-row sampling.

### Phase 4: Authoring workflow

- Add isolated-rule and endpoint-policy test APIs.
- Integrate the APIs into the Portal CEL editor.
- Support sanitized captured-context import by diagnostic ID.
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

- built-in sensitive headers are always redacted
- configured header and JSON-path masks are applied
- strict diagnostics never expose roots unavailable to strict CEL
- response bodies remain absent by default
- ordinary callers cannot enable or retrieve diagnostics
- diagnostic sessions expire and enforce event and byte limits
- truncation flags are correct

### Parity tests

- rule-test and runtime execution return the same outcome for the same rule,
  context, profile, and policy revision
- diagnostic collection does not change action mutation or short-circuiting
- cached MCP tools-list decisions are labeled as cached and are not reported as
  fresh evaluations

### Performance tests

- disabled diagnostics have no material request-path allocation regression
- denied-only and targeted sessions remain within defined latency budgets
- large contexts, large rows, and tools-list fan-out remain bounded

## Open Questions

1. Should the initial operator surface be a module-registry management tool, an
   HTTP management endpoint, or both?
2. Should detailed traces live only in structured logs, or also in a bounded
   in-memory buffer addressable by `diagnosticId`?
3. Does the selected CEL crate expose stable AST or observer APIs for reference
   extraction and per-node evaluation, or should Light-Fabric initially provide
   only best-effort static paths?
4. Which claims and tool-argument paths need installation-specific masks beyond
   the built-in secret-key rules?
5. Should production captured-context import into the Portal require a second
   approval beyond ordinary diagnostic access?
6. What retention and audit requirements apply when a diagnostic contains
   personal claims or business input after masking?

## Recommended Default

Implement structured outcomes first, then add an opt-in `referenced` diagnostic
session and a rule-test API. Do not begin with full-context denial logging.

This sequence fixes the information-loss problem, gives authors a safe way to
test rules before deployment, and leaves room for richer CEL expression tracing
if the evaluator later exposes a trustworthy observer API.
