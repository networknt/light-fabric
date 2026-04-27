# Light-Rule Design

Light-Rule is the local YAML rule engine used by Light-Fabric services and workflows for deterministic business checks, transformations, authorization decisions, and workflow assertions.

It complements agentic workflow by keeping critical decisions explicit, repeatable, and auditable. Agents can propose or select rules, but the rule engine executes the deterministic logic.

## Purpose

Light-Rule is designed for enterprise services that need fast local policy and transformation logic without a database call on every request.

Primary uses:

- fine-grained authorization
- request transformation
- response transformation
- workflow assertions
- business validation
- permission and filter injection
- reusable rule templates selected from Light-Portal

The rule configuration is loaded locally by the target service. When permissions or rule mappings change, the controller can trigger a config reload so the service swaps to the latest rules.

## Relationship To Agentic Workflow

Agentic Workflow orchestrates process steps. Light-Rule evaluates deterministic logic inside those steps.

Workflow uses Light-Rule in two main ways:

1. **Rule call task**
   A workflow task can call a named rule to validate or mutate workflow context.

2. **Assert task extension**
   Simple checks can be handled directly by `assert`, while complex business checks can delegate to Light-Rule.

This separation keeps workflows readable. The workflow says when a check happens; Light-Rule defines the reusable business logic for the check.

Example workflow responsibilities:

- decide when authorization configuration is needed
- select or create a rule
- invoke a rule during live testing
- route failures to a human or agent

Example Light-Rule responsibilities:

- evaluate role, group, position, or attribute checks
- inject endpoint permissions into the context
- compute row or column filters
- execute transformation plugins
- return pass/fail for business assertions

See [Agentic Workflow Design](agentic-workflow.md) for the workflow orchestration model.

## Relationship To LightAPI

LightAPI endpoint descriptions describe endpoint invocation and expected result behavior. Light-Rule can implement complex result checks that are too business-specific for simple schema assertions.

Recommended model:

- LightAPI describes endpoint result cases and expected behavior.
- Agentic Workflow invokes the endpoint and runs `assert` tasks.
- `assert` handles simple checks directly.
- Light-Rule handles complex checks, authorization logic, row filters, column filters, and reusable business policies.

See [LightAPI Description Design](lightapi-description.md) for endpoint capability descriptions.

## Rule Specification

Rules are described by the rule specification in `rule-specification/schema/rule.yaml`.

The top-level configuration contains:

- `ruleBodies`: named rule definitions
- `endpointRules`: endpoint-to-rule mappings

Each rule can contain:

- `ruleId`
- `ruleDesc`
- `version`
- `author`
- `updatedAt`
- `conditions`
- `actions`

Each endpoint mapping can contain:

- `req-tra`: request transformation rules
- `res-tra`: response transformation rules
- `access-control`: access control rules
- `permission`: permission values injected into context
- `x-*`: extension rule phases

## Rule Conditions

Conditions evaluate fields in the input context.

Supported operand forms:

- direct field: `role`
- dotted path: `user.role`
- JSON Pointer: `/user/role`
- JSONPath-like path: `$.user.roles[0]`

Supported operators:

```text
==
!=
>
<
>=
<=
eq
ne
contains
matches
startsWith
endsWith
exists
notExists
```

`expected` is typed and may be a string, number, boolean, array, object, or null.

Flat condition arrays are evaluated left-to-right. `joinCode` combines the current condition with the previous result.

```text
A AND B OR C
```

is evaluated as:

```text
(A AND B) OR C
```

If explicit grouping is required, split logic into multiple rules and combine them through endpoint mapping or workflow orchestration.

## Rule Actions

Actions execute plugin logic after conditions pass.

An action contains:

- `actionId`
- `actionClassName`
- `actionValues`

`actionClassName` identifies the registered plugin. `actionValues` carries plugin-specific configuration.

Typical action plugins:

- add values to request context
- inject permission attributes
- compute filters
- transform request body
- transform response body
- call a local business function

Actions are intentionally plugin-based so the schema remains stable while implementation logic can evolve.

## Endpoint Rule Phases

Endpoint mappings define when rules run.

### Request Transformation

`req-tra` rules run before the service handles the request. They can enrich or transform request context.

### Response Transformation

`res-tra` rules run after the service produces a response. They can filter, redact, or reshape response data.

### Access Control

`access-control` rules validate whether a request is allowed. These rules normally run in parallel because they should not mutate shared state.

### Permission Injection

`permission` values are injected into the evaluation context before rule execution. This lets API owners configure roles, groups, attributes, row filters, or column filters without editing the technical rule body.

### Extension Phases

Custom phases must use the `x-*` prefix. This avoids silent typos in standard phase names while preserving controlled extensibility.

## Execution Model

The Rust implementation lives in `crates/light-rule`.

Core components:

- `RuleConfig`: top-level config model
- `Rule`: rule definition
- `RuleCondition`: condition model
- `RuleAction`: action model
- `RuleEngine`: evaluates one rule
- `ActionRegistry`: maps action class names to plugins
- `MultiThreadRuleExecutor`: executes rule lists and endpoint phase mappings

Sequential phases such as `req-tra` and `res-tra` should run with `all` semantics so transformations happen in order.

Access control can run in parallel because it should be a validation step rather than a mutation step.

## Why Not Replace With Cedar Or Casbin

Cedar and Casbin are strong policy engines, but Light-Rule has a different role in this platform.

Light-Rule supports:

- local YAML configuration
- request and response transformation
- permission injection
- row and column filters
- endpoint-specific rule selection
- technical-team-authored reusable rules
- API-owner-selected rule parameters
- config reload through controller

Cedar is excellent for authorization policy, but it does not naturally cover transformation, row filter, and column filter use cases. Casbin is strong for policy enforcement, but it introduces a different policy storage and matching model.

Light-Rule should remain the native rule engine for Light-Fabric service configuration and workflow assertions. External policy engines can still be integrated as action plugins if needed.

## Governance

Rule bodies should be authored and reviewed like code or controlled configuration.

Recommended governance metadata:

- `version`
- `author`
- `updatedAt`
- `ruleDesc`

Recommended operational controls:

- validate rule YAML against the schema before publishing
- reject endpoint phase typos
- keep `ruleId` equal to the `ruleBodies` map key
- audit rule publication and reload events
- test rules with representative input contexts
- use workflow live tests to verify rules in integrated environments

## Workflow Live Testing

Light-Rule is useful in live tests because it can express business checks that are more specific than generic JSON assertions.

Example flow:

1. Workflow invokes an endpoint using LightAPI description.
2. Workflow captures the endpoint response.
3. `assert` verifies simple fields.
4. A rule task validates business-specific behavior.
5. On failure, workflow creates a task for a human or agent to investigate.

This keeps live test orchestration in workflow while preserving reusable business rules in Light-Rule.

## Design Rule

Use workflow for process control. Use LightAPI for endpoint capability. Use Light-Rule for deterministic business logic.

Agents may select, explain, or help author rules, but the rule engine should execute the final deterministic decision.
