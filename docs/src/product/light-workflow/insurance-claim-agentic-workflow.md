# Insurance Claim Agentic Workflow

This page describes a product workflow demo for orchestrating multiple agents,
skills, APIs, and human tasks with `light-workflow`.

The scenario is an auto insurance claim from first notice of loss to a
settlement recommendation. It is a useful demo because it is familiar, has clear
business states, needs several API calls, and includes human decisions that
should not be delegated fully to an agent.

## Demo Goal

The workflow should show how a deterministic process can coordinate:

- two or three agents
- multiple skills per agent
- REST API calls
- MCP tool calls through `light-gateway`
- human input and approval tasks
- branching based on policy, risk, and claim severity

The same business flow should be executable in two variants:

- **REST workflow**: calls the demo APIs directly with HTTP/OpenAPI tasks.
- **MCP workflow**: calls the same capabilities through MCP tools exposed by
  `light-gateway`.

The workflow owns the process. Agents work inside bounded tasks and should not
invent new process paths outside the workflow definition.

For the agent execution boundary, see
[Native Agent Call](native-agent-call.md). In the current implementation,
`call: agent` is a native `light-workflow` task. It does not invoke a
containerized `light-agent` service. API access in this demo is owned by the
workflow through direct HTTP tasks or MCP tool calls routed through
`light-gateway`.

## Execution Model

This demo uses the enterprise workflow-first model:

- `light-workflow` owns the claim process, task state, retries, branching,
  human tasks, and audit trail.
- API access is explicit in the workflow as `call: http` or `call: mcp`.
- Native `call: agent` tasks perform bounded reasoning over workflow-owned
  context and must return structured output.
- Skills provide instructions, tool context, and workflow mappings, but they do
  not give an agent permission to invent unreviewed process paths.
- Containerized `light-agent` services are not invoked by this demo workflow.
  They remain the runtime for chat clients and future service-agent
  integration.

## Demo APIs

The existing demo APIs can be used as stand-ins for insurance services.

| API | Role in the claim workflow |
| --- | --- |
| `demo-customer-profile-api` | Policyholder profile, vehicle list, policy status, contact preference, prior claims. |
| `demo-offer-decision-api` | Claim triage, risk decision, settlement or repair recommendation. |

If more realism is needed later, the same workflow can add simulated services
for document storage, repair estimates, fraud review, or payment authorization.

## Agents

### Claim Intake Agent

The Claim Intake Agent owns first notice of loss collection and basic
validation.

Skills:

- collect accident facts
- validate required claim fields
- look up customer, policy, and vehicle data
- identify missing information
- summarize the claim for the next agent

Typical tools or API calls:

- get customer profile
- get customer policies
- get covered vehicles
- get prior claims

Human tasks:

- claimant confirms accident details
- claimant answers missing information questions
- claimant uploads or confirms photos, police report, and tow status

### Coverage And Liability Agent

The Coverage and Liability Agent checks whether the claim can continue and
whether a human adjuster must review it.

Skills:

- coverage eligibility check
- incident date versus policy period check
- vehicle coverage check
- liability and severity classification
- fraud or special investigation flagging

Typical tools or API calls:

- get policy status
- get prior claim history
- run triage decision
- run risk decision

Human tasks:

- adjuster reviews unclear liability
- adjuster confirms coverage exception handling
- special investigation team reviews high-risk claims

### Settlement Agent

The Settlement Agent prepares the next action and customer-facing explanation.

Skills:

- repair versus total-loss recommendation
- deductible explanation
- settlement recommendation
- customer message draft
- next-document request

Typical tools or API calls:

- get offer decision
- get customer contact preference
- create settlement recommendation

Human tasks:

- adjuster approves high-value payment
- claimant accepts repair or settlement path
- claimant requests callback or more review

## Claim Context And Handoffs

The workflow engine owns the claim state. Agents should be treated as stateless
workers that read the current claim context, perform a bounded task, and return
structured output.

Each major step enriches a shared claim context:

- intake adds normalized accident facts and missing information status
- customer lookup adds profile, policy, vehicle, and prior-claim data
- coverage review adds eligibility, deductible, liability, and risk signals
- triage adds severity, recommended path, and human-review requirements
- settlement adds the recommendation, explanation, and next actions

Handoffs between agents should happen through this workflow-owned context, not
through private agent memory. This keeps the process deterministic,
replayable, and auditable.

## Workflow Outline

### 1. Start Claim

Input:

```json
{
  "customerId": "CUST-001",
  "vehicleId": "VEH-001",
  "incidentDate": "2026-05-30",
  "accidentDescription": "Rear-ended at an intersection.",
  "location": "Ottawa, ON",
  "injuryReported": false,
  "vehicleDrivable": false
}
```

The workflow validates that `customerId`, `vehicleId`, `incidentDate`, and
`accidentDescription` are present.

### 2. Fetch Customer Context

The workflow calls the profile and policy capabilities to retrieve:

- customer identity
- policy list
- covered vehicles
- contact preference
- prior claim count

Assertions:

- customer exists
- vehicle belongs to customer
- at least one active policy exists

### 3. Ask For Missing Information

If the input is incomplete, the workflow creates a human task for the claimant.

Example questions:

- Was anyone injured?
- Was another vehicle involved?
- Is the vehicle drivable?
- Was a police report filed?
- Are photos available?

The workflow should be resumable after the claimant answers.

### 4. Coverage Check

The workflow passes the gathered claim context to a native Coverage and
Liability agent task. That task checks:

- policy active on incident date
- covered vehicle
- applicable coverage type
- deductible
- excluded conditions

Branches:

- no matching policy: route to adjuster review
- policy inactive: prepare denial draft for human review
- coverage found: continue to triage

### 5. Triage Decision

The workflow calls the decision API, either directly with HTTP or through
`light-gateway` MCP, with normalized claim context.

Expected decision output:

```json
{
  "severity": "medium",
  "riskLevel": "low",
  "recommendedPath": "repair",
  "requiresAdjusterReview": false,
  "estimatedLoss": 3200
}
```

Branches:

- low risk and low value: continue automatically
- unclear liability: create adjuster review task
- high risk: create special investigation task
- high value: create approval task

### 6. Settlement Recommendation

The workflow passes the approved claim context to a native Settlement agent
task. That task prepares:

- recommended path: repair, estimate, total-loss review, denial draft, or more
  information
- deductible explanation
- next documents required
- customer-facing summary

The result should be structured so the UI can render it and the agent can
explain it.

### 7. Human Approval

Approval is required for:

- high estimated loss
- denial recommendation
- special investigation referral
- liability uncertainty
- customer dispute

The task should record:

- approver role
- approval decision
- comment
- timestamp
- whether the workflow should proceed, revise, or stop

### 8. Customer Response

The claimant chooses one of:

- accept repair path
- request adjuster callback
- upload more documents
- dispute the recommendation

This should be modeled as a human `ask` task rather than an agent-only step.

### 9. End State

Possible workflow outcomes:

| State | Meaning |
| --- | --- |
| `claim-approved` | Claim can proceed to repair or settlement. |
| `needs-adjuster-review` | Human adjuster must review before next action. |
| `needs-customer-info` | Claimant must provide missing information. |
| `referred-to-siu` | Claim is referred to special investigation. |
| `claim-denied-draft` | Denial is drafted but still needs human approval. |

## Failure Handling And Fallbacks

The demo should show graceful degradation when an API call or agent task cannot
finish automatically.

Recommended fallback behavior:

| Failure | Workflow response |
| --- | --- |
| Customer profile returns `404` | Create a manual customer verification task. |
| Policy or vehicle lookup is unavailable | Retry, then route to adjuster review with the partial claim context. |
| Decision API is unavailable | Create a manual triage task and include the last successful context. |
| Agent output fails validation | Re-run once with validation feedback, then create a human review task. |
| Human task times out | Escalate to the configured role or mark the claim as waiting for follow-up. |

The failure branch should preserve the accumulated claim context and the failed
request or response metadata so the human reviewer can continue from the same
state instead of restarting the claim.

## REST Variant

The REST workflow calls the demo APIs directly.

Use this variant to show:

- deterministic API orchestration
- direct HTTP/OpenAPI task execution
- workflow assertions
- human waiting tasks
- repeatable headless tests with fixed inputs

Example task sequence:

```text
start-claim
get-customer-profile
assert-active-policy
ask-missing-info
run-claim-triage
switch-risk-path
ask-adjuster-approval
prepare-settlement-summary
ask-customer-response
complete-claim
```

## MCP Variant

The MCP workflow invokes the same capabilities through MCP tools exposed by
`light-gateway`.

Use this variant to show:

- tool discovery with `tools/list`
- tool execution with `tools/call`
- agent skill guidance over the selected tool set
- gateway as the runtime MCP data plane

Skills should be treated as guidance and curation for the agent, not as the
runtime transport. The workflow still calls MCP tools through `light-gateway`.
A skill describes when and how to use tools. For example, the
`coverage-review` skill can instruct the agent to call `evaluate_coverage`
before `score_claim_risk`, explain which fields must be present, and define
what output shape the workflow expects.

Example tool groups:

| Skill | Tools |
| --- | --- |
| `claim-intake` | `get_customer_profile`, `get_policy`, `get_vehicle`, `list_prior_claims` |
| `coverage-review` | `evaluate_coverage`, `score_claim_risk`, `classify_liability` |
| `settlement` | `recommend_offer`, `generate_customer_summary`, `list_required_documents` |

## Human Task Model

Human work should be explicit and durable.

Recommended task types:

- claimant information request
- adjuster approval
- liability review
- special investigation review
- customer settlement response

Recommended fields:

- prompt
- mode: choice, text, object, file, approval
- assignee or candidate role
- due time
- validation rules
- sensitive flag
- comments
- decision result

The workflow should pause at the human task and resume after a valid response is
recorded.

The pause is durable. `light-workflow` persists the process and task state while
waiting, so the workflow can remain idle for hours or days without consuming
active execution resources. When the claimant, adjuster, or investigator
completes the task, the workflow resumes from the persisted state and continues
with the same claim context.

## Minimal First Implementation

Start with a narrow happy path:

1. Start with `customerId`, `vehicleId`, and accident details.
2. Workflow fetches customer profile through HTTP or MCP.
3. Workflow asserts active policy and covered vehicle.
4. Workflow calls the decision API for triage.
5. Workflow asks an adjuster to approve if `estimatedLoss` exceeds a threshold.
6. Native Settlement agent task prepares the recommendation.
7. Workflow completes with `claim-approved` or `needs-adjuster-review`.

This first version is enough to demonstrate multi-agent orchestration without
needing every insurance edge case.

## Later Enhancements

Add complexity incrementally:

- document upload and OCR simulation
- repair shop estimate comparison
- fraud and special investigation path
- payment authorization
- subrogation when another driver is liable
- scheduled headless regression runs
- customer notification drafting
- analytics for cycle time and approval bottlenecks

## Demo Success Criteria

The demo is successful if it shows:

- the same business process running through REST and MCP variants
- agents using skills to perform bounded work
- APIs called through both direct HTTP and MCP tool paths
- at least one human input task
- at least one human approval task
- auditable workflow state transitions
- clear final outcome and explanation
