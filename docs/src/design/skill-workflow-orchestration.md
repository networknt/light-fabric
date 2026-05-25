# Skill Workflow Orchestration

## Status

Proposed demo design.

## Executive Summary

This design describes a focused demo for agent-driven orchestration in
Light-Fabric. The demo uses one agent with two skills:

1. A skill that starts a workflow which calls two REST APIs directly.
2. A skill that starts a workflow which calls the same two REST APIs through
   the MCP router.

Both paths solve the same business use case and return the same output. The
visible difference is the execution trace:

- The REST workflow shows `light-workflow` invoking HTTP endpoints directly.
- The MCP workflow shows `light-workflow` invoking MCP `tools/call`, with
  `light-gateway` routing each tool call to the same backend REST APIs.

This demonstrates that skills provide agent-facing guidance and discovery,
workflows provide durable orchestration, and the gateway provides the MCP data
plane for tool execution.

## Goals

- Show one agent selecting between two assigned skills.
- Show a workflow that orchestrates multiple REST APIs directly.
- Show a second workflow that orchestrates the same APIs through MCP tools.
- Keep the input and output contract identical across both workflows.
- Keep the demo small enough to explain in a few minutes.
- Preserve the runtime boundary: skills guide, workflows orchestrate, gateway
  executes MCP tool calls.

## Non-Goals

- Do not benchmark REST versus MCP latency.
- Do not claim that MCP replaces REST. The demo shows two supported access
  patterns over the same backend capabilities.
- Do not require every skill to be workflow-backed. Simple skills can remain
  instructions plus allowed tools.
- Do not move MCP tool execution into the portal registry or agent catalog.
  Runtime tool execution stays on the gateway `tools/call` path.
- Do not make the demo depend on a large endpoint catalog.

## Recommendation

Use two APIs, not one.

A one-API demo can show sequencing, but it does not clearly prove
cross-service orchestration. Two APIs show a more realistic enterprise shape:
the workflow has to collect data from one business capability and make a
decision through another capability.

Use four endpoints for the base demo.

| Demo size | Endpoint count | Recommendation | Why |
| --- | ---: | --- | --- |
| Smoke test | 2 | Optional only | Shows a happy path, but not enough variation. |
| Base demo | 4 | Recommended | Covers path parameters, query parameters, arrays, request bodies, branching, and transformation. |
| Advanced demo | 6 | Later phase | Adds parallel enrichment, compensation, or audit callbacks. |

The base demo should be small enough to run repeatedly while still proving
meaningful orchestration behavior.

## Demo Scenario

The demo domain is personalized offer recommendation.

The agent receives a prompt such as:

```text
Recommend an offer for customer CUST-1001.
```

The agent can use either skill:

- `Personalized Offer via REST Workflow`
- `Personalized Offer via MCP Router`

If the prompt does not specify REST or MCP, the demo agent should not pick a
path at random. It should ask a short clarification question:

```text
Do you want to run this through the direct REST workflow or through the MCP
router workflow?
```

Scripted demos can avoid the clarification by naming the path in the prompt.

Both skills start a workflow that:

1. Loads the customer profile.
2. Loads customer preferences and consent.
3. Stops if the customer has not consented.
4. Searches for eligible offers.
5. Selects the best offer.
6. Records the offer decision.
7. Returns a normalized decision payload.

## APIs And Endpoints

### Customer Profile API

The Customer Profile API owns customer data and preferences.

| Endpoint | Shape | Purpose |
| --- | --- | --- |
| `GET /customers/{customerId}` | Path parameter, object response | Load customer identity, segment, region, and account status. |
| `GET /customers/{customerId}/preferences?channel=portal` | Path parameter plus query parameter | Load consent, preferred categories, and contact channel rules. |

### Offer Decision API

The Offer Decision API owns eligible offer lookup and decision recording.

| Endpoint | Shape | Purpose |
| --- | --- | --- |
| `GET /offers?segment={segment}&state={state}&category={category}` | Query parameters, array response | Search active offers matching the customer profile and preferences. |
| `POST /offer-decisions` | JSON request body, object response | Persist the selected offer decision and return a decision id. |

## Demo API Runtime Services

The two business APIs should be implemented as real Rust services using the
`light-axum` framework, not as ad hoc mocks. This keeps the demo aligned with
normal Light-Fabric service lifecycle behavior:

- load runtime configuration from config-server
- bind HTTP using configured server settings
- register with controller through `portal-registry`
- appear in the control panel service-discovery view
- support gateway service discovery by `serviceId` and `envTag`

Recommended demo apps:

| App | Service id | Default HTTP port | Purpose |
| --- | --- | ---: | --- |
| `demo-customer-profile-api` | `com.networknt.demo.customer-profile-1.0.0` | `8085` | Serves customer profile and preference data. |
| `demo-offer-decision-api` | `com.networknt.demo.offer-decision-1.0.0` | `8086` | Serves offer lookup and decision recording. |

The ports are config defaults only. They must be configurable through
config-server values so local, Docker, Kubernetes, and shared demo
environments can choose different ports without recompiling.

Both services should expose:

```text
GET /health
```

The API endpoints should return deterministic demo data. A database is not
required for the first demo; in-memory seed data is enough as long as the data
is stable and documented. If later demos need persistence, keep it behind the
same endpoint contract.

### Light-Axum Bootstrap

Each demo API should follow the normal `light-axum` pattern: implement
`AxumApp`, return an `axum::Router`, and let `LightRuntimeBuilder` own binding,
configuration, shutdown, and controller registration.

The service should read config from the same runtime config files used by other
Light-Fabric services:

```text
startup.yml
server.yml
portal-registry.yml
```

Example config-server values for the Customer Profile API:

```yaml
startup.host: dev.lightapi.net
startup.externalConfigDir: /var/lib/demo-customer-profile-api/config-cache

light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435

server.serviceId: com.networknt.demo.customer-profile-1.0.0
server.environment: demo
server.ip: 0.0.0.0
server.advertisedAddress: demo-customer-profile-api
server.httpPort: 8085
server.enableHttp: true
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438
```

Example config-server values for the Offer Decision API:

```yaml
startup.host: dev.lightapi.net
startup.externalConfigDir: /var/lib/demo-offer-decision-api/config-cache

light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435

server.serviceId: com.networknt.demo.offer-decision-1.0.0
server.environment: demo
server.ip: 0.0.0.0
server.advertisedAddress: demo-offer-decision-api
server.httpPort: 8086
server.enableHttp: true
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438
```

`server.advertisedAddress` must be a reachable address, not `0.0.0.0`. In
Kubernetes, use the Service DNS name. In local Docker Compose, use the Compose
service name. In a native VM demo, use the VM hostname or another reachable
address.

### Controller Registration

The services should register with controller using the runtime's
`portal-registry` integration. The controller registration payload must include
at least:

- `serviceId`
- `envTag`
- protocol
- advertised address
- port
- discovery token or portal registry token, according to environment policy

After startup, the control panel should show two registered service instances:

```text
com.networknt.demo.customer-profile-1.0.0 / demo
com.networknt.demo.offer-decision-1.0.0 / demo
```

The MCP router configuration should prefer these service IDs over fixed
`targetHost` values where service discovery is available. Fixed `targetHost`
values are still useful for a minimal local smoke test.

## Optional Advanced Endpoints

The base demo should start with four endpoints. If we later want to demonstrate
more workflow shapes, add one or two optional endpoints:

| Endpoint | Shape Demonstrated | Use |
| --- | --- | --- |
| `GET /customers/{customerId}/risk` | Parallel enrichment | Run profile, preferences, and risk lookup before offer selection. |
| `POST /offer-decisions/{decisionId}/audit` | Follow-up side effect | Record a compliance audit event after the decision is created. |
| `POST /offer-decisions/{decisionId}/cancel` | Compensation | Cancel the decision if a later step fails. |

## Agent, Skills, And Workflows

Use one agent so the demo highlights skill selection rather than agent
handoff.

| Object | Name | Responsibility |
| --- | --- | --- |
| Agent | `Demo Orchestration Agent` | Receives the user request and selects one of the assigned skills. |
| Skill | `Personalized Offer via REST Workflow` | Guides the agent to start the direct REST workflow. |
| Skill | `Personalized Offer via MCP Router` | Guides the agent to start the MCP-backed workflow. |
| Workflow | `personalized-offer-rest-v1` | Orchestrates direct HTTP calls to the two REST APIs. |
| Workflow | `personalized-offer-mcp-v1` | Orchestrates MCP tool calls through the gateway router. |

The skill registry should link each skill to its workflow definition through
`skill_workflow_t`. The workflow definition remains canonical in
`wf_definition_t.definition`. The skill `content_markdown` remains
agent-facing guidance, not the executable workflow source.

## Execution Paths

### Direct REST Workflow

```text
User prompt
  -> Demo Orchestration Agent
  -> Personalized Offer via REST Workflow skill
  -> light-workflow
  -> Customer Profile API
  -> Offer Decision API
  -> normalized decision result
```

This path is useful for showing direct, durable API orchestration.

### MCP Router Workflow

```text
User prompt
  -> Demo Orchestration Agent
  -> Personalized Offer via MCP Router skill
  -> light-workflow
  -> MCP tools/call
  -> light-gateway MCP router
  -> Customer Profile API
  -> Offer Decision API
  -> normalized decision result
```

This path is useful for showing MCP protocol orchestration over the same
backend API capabilities.

## Common Workflow Contract

Both workflows should accept the same input:

```json
{
  "customerId": "CUST-1001",
  "channel": "portal"
}
```

Both workflows should return the same successful output shape:

```json
{
  "status": "APPROVED",
  "customerId": "CUST-1001",
  "selectedOfferId": "OFFER-TRAVEL-01",
  "decisionId": "DEC-1001"
}
```

Both workflows should return comparable business outcomes for known edge
cases:

```json
{
  "status": "NO_CONSENT",
  "customerId": "CUST-3003",
  "reason": "Customer has not consented to personalized offers."
}
```

```json
{
  "status": "NO_ELIGIBLE_OFFER",
  "customerId": "CUST-2002",
  "reason": "No active offer matches the customer profile and preferences."
}
```

## Workflow Shape

The REST and MCP workflows should have the same logical steps.

| Step | REST workflow action | MCP workflow action |
| --- | --- | --- |
| Load profile | `GET /customers/{customerId}` | `tools/call customer_get_profile` |
| Load preferences | `GET /customers/{customerId}/preferences` | `tools/call customer_get_preferences` |
| Check consent | Workflow condition | Workflow condition |
| Search offers | `GET /offers` | `tools/call offer_search` |
| Select offer | Workflow expression or rule | Workflow expression or rule |
| Record decision | `POST /offer-decisions` | `tools/call offer_record_decision` |
| Return result | Workflow output mapping | Workflow output mapping |

The workflow should own branching, retries, and output normalization. The agent
should not manually sequence each API call after the workflow starts.

## Error Handling And Retries

Business outcomes and technical failures should be treated differently.

Business outcomes are expected workflow results and should not be retried:

- `NO_CONSENT`
- `NO_ELIGIBLE_OFFER`

Technical failures should use bounded workflow retries:

| Failure | Recommended behavior |
| --- | --- |
| Customer Profile API timeout | Retry the profile step with exponential backoff. |
| Offer Decision API returns `503` | Retry the affected offer step with exponential backoff. |
| Gateway MCP `tools/call` timeout | Retry the MCP tool-call step with the same workflow policy. |
| Persistent downstream failure | End with a controlled technical failure result and preserve the workflow trace. |

Recommended transient retry status codes:

```text
408, 429, 502, 503, 504
```

The `POST /offer-decisions` step should include an idempotency key derived from
the workflow instance id and selected offer id. This prevents duplicate
decisions when a retry happens after the backend processed the first request
but the response was lost.

For parity, the REST and MCP workflows should use the same retry policy. In the
MCP path, the gateway should preserve enough error detail for the workflow
trace to show the tool name, mapped backend endpoint, status code, and
correlation id.

## MCP Tool Mapping

The MCP workflow should use a small, explicit tool set.

| MCP tool | Backend endpoint | Arguments |
| --- | --- | --- |
| `customer_get_profile` | `GET /customers/{customerId}` | `customerId` |
| `customer_get_preferences` | `GET /customers/{customerId}/preferences` | `customerId`, `channel` |
| `offer_search` | `GET /offers` | `segment`, `state`, `category` |
| `offer_record_decision` | `POST /offer-decisions` | `customerId`, `offerId`, `channel`, `source`, `reason` |

The MCP tool input schemas should be normalized JSON objects. The gateway
router maps those objects to path parameters, query parameters, or request
bodies for the backend REST APIs.

The MCP skill should list these tools in `skill_tool_t` as its allowed runtime
tool set. Workflow validation should flag an MCP tool-call step if it references
a tool that is not linked to the skill.

### Gateway Tool Configuration Example

Current gateway HTTP tool execution maps GET arguments to query parameters and
sends non-GET arguments as JSON request bodies. To support endpoint shapes such
as `GET /customers/{customerId}` without changing the backend API, the demo
should add or configure explicit path-template substitution before the request
is sent.

Recommended minimal mapping shape:

```yaml
mcp-router.tools:
  - name: customer_get_profile
    description: Get a customer profile by id.
    protocol: http
    serviceId: com.networknt.demo.customer-profile-1.0.0
    envTag: demo
    path: /customers/{customerId}
    method: GET
    apiType: http
    inputSchema:
      type: object
      required:
        - customerId
      properties:
        customerId:
          type: string
    toolMetadata:
      pathParams:
        - customerId
```

With this mapping, the MCP tool call:

```json
{
  "name": "customer_get_profile",
  "arguments": {
    "customerId": "CUST-1001"
  }
}
```

should be routed to:

```text
GET /customers/CUST-1001
```

The path parameter should not also be appended as a query parameter. Arguments
not listed under `pathParams` can still be appended as query parameters for GET
requests or sent as JSON body fields for POST requests.

## Skill Content Markdown Guidance

The skill `content_markdown` should explain when and how the agent should use
the skill. It should not duplicate the workflow definition or the full API
contract.

Example REST skill content:

```markdown
## Purpose
Use this skill when the user asks for a personalized offer decision through the
direct REST workflow.

## Inputs
- customerId: customer identifier, such as CUST-1001
- channel: request channel, default portal

## Behavior
- Start workflow personalized-offer-rest-v1.
- Return the workflow result as the answer.
- Do not manually call offer APIs outside the workflow.
- If the user does not specify REST or MCP, ask which execution path they want.
```

Example MCP skill content:

```markdown
## Purpose
Use this skill when the user asks to demonstrate MCP router orchestration for a
personalized offer decision.

## Inputs
- customerId: customer identifier, such as CUST-1001
- channel: request channel, default portal

## Behavior
- Start workflow personalized-offer-mcp-v1.
- The workflow will call MCP tools through the gateway.
- Return the workflow result as the answer.
- If the user does not specify REST or MCP, ask which execution path they want.
```

Structured execution metadata belongs in registry rows and workflow
definitions, not only in markdown. The markdown is the LLM-facing explanation.

## Output Normalization

The workflows should not pass raw endpoint responses directly to the agent.
They should normalize backend responses into a stable business result.

Example raw `POST /offer-decisions` response:

```json
{
  "decisionId": "DEC-1001",
  "customerId": "CUST-1001",
  "offerId": "OFFER-TRAVEL-01",
  "decision": "approved",
  "createdAt": "2026-05-25T14:12:00Z",
  "auditRef": "AUD-7788"
}
```

Normalized workflow output:

```json
{
  "status": "APPROVED",
  "customerId": "CUST-1001",
  "selectedOfferId": "OFFER-TRAVEL-01",
  "decisionId": "DEC-1001"
}
```

The workflow should own this transformation so the REST and MCP variants
produce identical final results even if their intermediate transport envelopes
are different.

## Demo Data

Use deterministic seed data so the demo is repeatable.

| Customer | Profile | Preferences | Expected result |
| --- | --- | --- | --- |
| `CUST-1001` | Premium segment, active, Ontario | Consent true, travel preferred | `APPROVED` with `OFFER-TRAVEL-01`. |
| `CUST-2002` | Standard segment, active, Ontario | Consent true, travel preferred | `NO_ELIGIBLE_OFFER`. |
| `CUST-3003` | Premium segment, active, Ontario | Consent false | `NO_CONSENT`. |

Seed offers:

| Offer | Match condition | Result |
| --- | --- | --- |
| `OFFER-TRAVEL-01` | `segment=premium`, `state=ON`, `category=travel` | Eligible for `CUST-1001`. |
| `OFFER-CASHBACK-01` | `segment=premium`, `state=BC`, `category=shopping` | Not eligible for Ontario travel scenario. |

## Demo Script

Run the REST workflow path first:

```text
Use the REST workflow skill to recommend an offer for CUST-1001.
```

Expected observation:

- The agent selects `Personalized Offer via REST Workflow`.
- The workflow trace shows direct HTTP calls to the Customer Profile API and
  Offer Decision API.
- The final response contains `status=APPROVED` and a decision id.

Run the MCP workflow path second:

```text
Use the MCP router skill to recommend an offer for CUST-1001.
```

Expected observation:

- The agent selects `Personalized Offer via MCP Router`.
- The workflow trace shows MCP `tools/call` invocations.
- The gateway trace shows those tool calls routed to the same backend REST
  endpoints.
- The final response uses the same output shape as the REST workflow.

Then run one edge case:

```text
Use either skill to recommend an offer for CUST-3003.
```

Expected observation:

- The workflow stops after the consent check.
- No offer decision is recorded.
- The result is `NO_CONSENT`.

Run one ambiguity case:

```text
Recommend an offer for CUST-1001.
```

Expected observation:

- The agent asks whether to use the direct REST workflow or the MCP router
  workflow.
- After the user chooses, the agent starts the selected workflow.

Run one technical failure case:

```text
Use the MCP router skill to recommend an offer for CUST-1001 while the Offer
Decision API returns one transient 503.
```

Expected observation:

- The workflow retries the failed tool-call step.
- The gateway trace records the failed `offer_record_decision` call and the
  successful retry.
- The final response still uses the normalized `APPROVED` output shape.

## Portal Authoring Flow

The portal should make the demo visible from the existing GenAI and workflow
surfaces:

1. Create or import the two REST APIs and four endpoint descriptions.
2. Implement the two APIs as `light-axum` services.
3. Add config-server values for both API services.
4. Start both services and verify controller registration.
5. Publish MCP router tools for the same four endpoints.
6. Create `personalized-offer-rest-v1` in the workflow catalog.
7. Create `personalized-offer-mcp-v1` in the workflow catalog.
8. Create the two skills in the skill registry.
9. Link each skill to its primary workflow through `skill_workflow_t`.
10. Link the MCP skill to its allowed tool set through `skill_tool_t`.
11. Assign both skills to `Demo Orchestration Agent`.
12. Use Skill Workspace preview and test panels to validate the effective prompt,
   workflow link, allowed tools, and sample test input.

## Validation Rules

The authoring experience should validate the following before the demo is
considered complete:

- Each skill has exactly one primary workflow link.
- The REST workflow does not require MCP tools.
- The MCP workflow references only MCP tools linked through `skill_tool_t`.
- Both workflows declare the same input schema.
- Both workflows declare the same normalized output shape.
- The four backend endpoint descriptions are active.
- Both demo API services load config from config-server.
- Both demo API services register with controller and appear in the control
  panel service-discovery view.
- The MCP router `tools/list` result includes the four expected tool names.
- MCP router tools resolve the demo APIs by `serviceId` and `envTag` in the
  service-discovery environment.
- MCP path-parameter mappings are validated before the workflow test run.
- `POST /offer-decisions` includes an idempotency key for retry safety.
- Test runs for `CUST-1001`, `CUST-2002`, and `CUST-3003` produce the expected
  outcomes.

## Observability

The demo should show three different traces:

1. Agent trace: which skill the agent selected and what workflow it started.
2. Workflow trace: step order, branches, retries, and final output.
3. Gateway trace: MCP tool name, mapped backend endpoint, status, duration, and
   correlation id for the MCP path.

Use the same correlation id across the agent request, workflow instance, and
gateway calls where possible. This makes the REST and MCP execution paths easy
to compare.

## Security And Authorization

Authorization should be enforced at each layer:

- The agent can discover only assigned skills.
- The workflow can start only definitions visible to the authenticated caller
  or service identity.
- The MCP skill can expose only tools linked to the skill and allowed for the
  agent.
- The gateway still performs runtime MCP access checks before executing
  `tools/call`.
- Backend REST APIs continue to enforce their own authorization policies.

The skill registry is not a runtime bypass. It narrows discovery and guidance,
while the workflow and gateway remain responsible for execution-time controls.

### Context And Auth Propagation

The demo should explicitly show that caller context is preserved.

For direct REST workflow steps:

1. The workflow start request records the initiating user, host, tenant,
   correlation id, and authorization context.
2. The workflow executor builds outbound REST calls with the correct caller
   context. If the original bearer token is safe to forward, it can be passed
   through. Otherwise, the workflow service should use a service token with
   on-behalf-of metadata that preserves the initiating subject.
3. Backend APIs enforce their normal authorization policies.

For MCP workflow steps:

1. `light-workflow` calls the gateway MCP endpoint with the same correlation,
   tenant, locale, and authorization context.
2. `light-gateway` validates the MCP request and runtime tool authorization.
3. The MCP router forwards the allowed caller headers to the backend REST API
   while regenerating transport-specific headers such as `Host`,
   `Content-Length`, and connection management headers.
4. Backend APIs see the same business identity context they would see on the
   direct REST path.

The trace should show this propagation without exposing sensitive token values.

## Acceptance Criteria

- One demo agent has both skills assigned.
- The REST skill starts `personalized-offer-rest-v1`.
- The MCP skill starts `personalized-offer-mcp-v1`.
- Both workflows accept the same input JSON.
- Both workflows return the same normalized output shape.
- The REST workflow trace shows direct REST calls to two APIs.
- The MCP workflow trace shows MCP `tools/call` routed through the gateway to
  the same two APIs.
- The two APIs run as `light-axum` services with config-server supplied HTTP
  ports.
- The two APIs register with controller and are visible in the control panel
  service-discovery view.
- The demo succeeds for `CUST-1001`.
- The demo returns controlled business outcomes for `CUST-2002` and
  `CUST-3003`.
- Ambiguous user prompts trigger a clarification question instead of random
  skill selection.
- A transient `503` from the Offer Decision API is retried and appears in the
  workflow trace.
- The MCP path preserves caller context through workflow, gateway, and backend
  REST calls.

## Related Designs

- [Agentic Workflow](agentic-workflow.md)
- [Workflow Client Architecture](workflow-client-architecture.md)
- [Centralized Skills](centralized-agent-skills.md)
- [LightAPI Description](lightapi-description.md)
- [MCP Router](mcp-router.md)
