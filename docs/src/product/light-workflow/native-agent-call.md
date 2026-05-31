# Native Agent Call

## Status

Recommended platform boundary.

`call: agent` is currently a native `light-workflow` task. It does not invoke a
running `light-agent` container. The workflow engine loads the portal agent
definition, selected skills, and skill tools from the database, builds a bounded
model prompt, calls the configured model provider directly, validates the JSON
output, and continues the workflow.

Containerized `light-agent` remains the interactive agent runtime. It serves
chat clients, keeps session memory, loads its effective catalog, and calls MCP
tools through `light-gateway`.

This page defines how both models should coexist in an enterprise platform.

## Problem

The platform has two useful agent execution models:

- native agent tasks inside `light-workflow`
- containerized `light-agent` services

Both can use the same portal-authored concepts: agent definitions, skills,
tools, workflow mappings, and gateway-routed API capabilities. They should not
be treated as interchangeable runtime paths.

The main design question is whether a workflow should keep executing
`call: agent` natively or call a containerized `light-agent` service for every
agent step.

## Current Behavior

When a workflow contains:

```yaml
do:
  - review-offer:
      call: agent
      with:
        agent: com.networknt.agent.offer-1.0.0
        skill: offer-decision
        input:
          customerId: "${ .customerId }"
          profile: "${ .profile }"
        outputSchemaRef: offerDecision
```

`light-workflow` handles the task itself:

1. Resolve the agent by `agent_def_id` or agent API name.
2. Load active skills assigned to the agent from `agent_skill_t`.
3. If a skill is specified, narrow the prompt to that skill.
4. Load skill tool metadata from `skill_tool_t`, `tool_t`, and `tool_param_t`.
5. Build a bounded prompt from workflow context, skill instructions, optional
   task instructions, and the expected output schema.
6. Call the model provider configured on the portal agent definition.
7. Parse and validate the model response as JSON.
8. Return the structured output to the workflow context.

The native task does not:

- call the `light-agent` HTTP or WebSocket endpoint,
- use `light-agent` session memory,
- let the model run a dynamic gateway tool loop,
- execute tool calls from the model response.

Skill tools are included as guidance and future-routing context. In the current
runtime phase, API orchestration remains explicit workflow tasks such as
`call: http`, `call: mcp`, `assert`, `switch`, and `ask`.

## Native Agent Tasks

Native agent tasks are best for bounded reasoning where the workflow remains
the system of record.

Good examples:

- classify a request,
- normalize user-provided input,
- summarize API results,
- choose between workflow branches,
- draft a customer-facing explanation,
- assess whether human approval is required,
- produce structured output that must match a schema.

Benefits:

- Strong auditability: workflow records input, output, status, retry, and
  failure state.
- Deterministic orchestration: API calls, approvals, assertions, and retries
  stay in the workflow definition.
- Easier governance: output schemas and workflow-owned context constrain the
  model.
- Lower operational coupling: the task does not depend on a separate agent
  service instance being healthy.
- Better replay and diagnostics: the workflow engine owns the execution state.

Tradeoffs:

- It is not the full `light-agent` runtime.
- It does not use chat session history or Hindsight memory.
- It can duplicate some prompt/catalog handling from `light-agent`.
- Model provider scaling is tied to `light-workflow`.
- Dynamic tool selection is intentionally limited.

## Containerized Agents

Containerized agents are independently deployed `light-agent` services.

They are best for interactive or autonomous agent behavior where the agent
runtime itself is the product surface.

Good examples:

- user-facing chat agents,
- long-lived specialist agents,
- agents that need session memory,
- agents that should cache and refresh their effective catalog locally,
- agents that need a dynamic `tools/list` and `tools/call` loop through
  `light-gateway`,
- agents that must scale independently from workflow execution.

Benefits:

- Real agent runtime behavior: memory, chat sessions, local catalog cache, and
  gateway tool execution.
- Independent deployment, scaling, health checks, and versioning.
- Clear service identity through controller registration.
- Better fit for interactive clients and long-running conversational work.

Tradeoffs:

- Harder workflow audit if the agent internally decides which APIs to call.
- More distributed failure modes: network errors, timeouts, retries, and
  partial progress.
- Requires strict request and response contracts.
- Requires idempotency, correlation IDs, auth scopes, and timeout policy.
- Can make the workflow less deterministic if the agent is allowed to run an
  open-ended tool loop.

## Recommendation

Keep the mixed approach, but make the boundary explicit.

Use native `call: agent` for bounded reasoning inside workflow-controlled
processes. Use workflow tasks and subworkflows for API orchestration. Use
containerized `light-agent` for interactive chat and specialist runtime agents.

The recommended enterprise pattern is:

```text
main workflow
  -> call: mcp or call: http for deterministic API access
  -> run/start subworkflow for reusable skill-backed API orchestration
  -> call: agent for bounded reasoning over workflow-owned context
  -> ask/assert/switch/retry/audit in workflow

chat client
  -> containerized light-agent
  -> effective catalog from portal-query
  -> tools/list and tools/call through light-gateway
  -> session memory and chat history
```

Do not route every workflow agent step through a containerized agent by
default. That would move too much process control into agent services and make
enterprise audit, replay, and approval harder.

Do not remove native `call: agent`. It is the right primitive for workflow-owned
reasoning steps.

## Skill To Workflow Pattern

For skills that require API orchestration, prefer mapping the skill to a
workflow or subworkflow.

Example:

```text
skill_t: customer-profile-review
  -> skill_workflow_t: customer-profile-enrichment-v1
  -> wf_definition_t: workflow that calls gateway MCP tools
```

In that pattern:

- the skill describes when and why to use the capability,
- the workflow owns the API call sequence,
- `light-gateway` executes MCP tool calls,
- native `call: agent` can summarize or classify the results,
- the workflow remains the audit boundary.

This is the preferred model for enterprise API access because it prevents an
agent from inventing an unreviewed process path.

## Demo Guidance

The current demos should be described precisely:

- `insurance-claim-rest-v1.yaml` shows workflow-owned API orchestration with
  direct HTTP calls plus native agent tasks for bounded reasoning.
- `insurance-claim-mcp-v1.yaml` shows the same business flow through
  `light-gateway` MCP tools plus native agent tasks for bounded reasoning.
- `insurance-claim-headless-v1.yaml` shows the deterministic regression path
  without human-task pauses.

The demos do not currently prove that `light-workflow` invokes the
containerized `light-agent` services. That can be added later as an explicit
runtime integration if the platform needs it.

## Future Containerized-Agent Invocation

If workflow needs to call containerized `light-agent` services in the future,
do not silently change the meaning of native `call: agent`. Add an explicit
mode or task contract so operators can see which runtime path is used.

Possible options:

```yaml
call: agent
with:
  mode: native
  agent: com.networknt.agent.offer-1.0.0
  skill: offer-decision
```

```yaml
call: agent
with:
  mode: service
  agent: com.networknt.agent.offer-1.0.0
  skill: offer-decision
  timeout: PT30S
```

or a separate task type:

```yaml
call: agent-service
with:
  serviceId: com.networknt.agent.offer-1.0.0
  envTag: dev
  skill: offer-decision
```

The service-call contract must require:

- explicit timeout and retry policy,
- idempotency key for side-effecting work,
- correlation and workflow instance headers,
- output schema validation,
- clear failure mapping to workflow status,
- portal/gateway authorization policy,
- observability across workflow, gateway, controller, and agent logs.

## Decision Matrix

| Need | Preferred runtime |
| --- | --- |
| Deterministic API sequence | Workflow task or subworkflow |
| Gateway-routed API access | `call: mcp` through `light-gateway` |
| Bounded model reasoning | Native `call: agent` |
| Human approval or form input | `ask` task |
| Policy assertion | `assert`, `switch`, or rule task |
| Interactive chat | Containerized `light-agent` |
| Session memory | Containerized `light-agent` |
| Dynamic tool loop | Containerized `light-agent` |
| Enterprise audit and replay | Workflow-owned task |

## Long-Term Direction

The platform should keep both execution models:

- Native agent tasks for workflow-owned reasoning.
- Containerized agents for interactive, memory-backed, independently scaled
  agent services.

The enterprise control rule is simple: workflows own durable process state and
auditable API orchestration; agents provide bounded reasoning or interactive
specialist behavior within contracts defined by the platform.
