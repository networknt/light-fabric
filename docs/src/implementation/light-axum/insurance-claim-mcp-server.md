# Insurance Claim MCP Server Example

This plan describes a new `light-example-rs` MCP server example for the
insurance claim workflow demo. The server should be built on `light-axum` so it
uses the same runtime startup, config-server bootstrap, service registration,
logging control, TLS, and graceful shutdown pattern as the existing REST demo
APIs.

## Source Review

The product workflow doc at
`docs/src/product/light-workflow/insurance-claim-agentic-workflow.md` defines two
execution variants:

- the REST variant calls demo APIs directly
- the MCP variant calls the same capabilities through `light-gateway`

The existing `light-example-rs` apps provide two REST APIs:

- `apps/demo-customer-profile-api`
- `apps/demo-offer-decision-api`

The current MCP workflow definition,
`apps/light-workflow/examples/insurance-claim-mcp-v1.yaml`, currently calls MCP
tools for capabilities that already exist as REST endpoints. That workflow
should be replaced. The new version should deliberately mix both integration
styles:

- REST calls remain responsible for existing demo API capabilities.
- MCP calls cover only the functional gaps that are not already implemented by
  the REST APIs.

## Functional Gaps

The workflow doc names a broader insurance tool set than the existing REST demo
APIs currently provide. The gaps fall into three groups.

### 1. No Native Backend MCP Server

`light-gateway` can expose REST APIs as MCP tools and can proxy backend MCP
servers, but `light-example-rs` does not yet include a backend MCP server. This
means the demo does not prove the end-to-end path:

```text
light-workflow call:mcp
  -> light-gateway /mcp
  -> backend MCP server built with light-axum
  -> tool implementation
```

The new example should fill this first.

### 2. Coverage And Liability Are Still Agent Mock Output

The workflow currently uses a native `coverage-liability-agent` task with
`mockOutput` for:

- coverage status
- liability status
- risk level
- estimated loss
- deductible
- adjuster review flag
- SIU review flag

The product doc lists coverage-review tools such as `evaluate_coverage`,
`score_claim_risk`, and `classify_liability`, but the concrete workflow does not
call those as MCP tools yet. A backend MCP server can make this part
deterministic and testable.

### 3. Settlement Support Is Still Too Coarse

The workflow uses a native `settlement-agent` with `mockOutput`, then calls
`recommendSettlement`. The existing offer decision API returns a settlement
recommendation, but it does not expose separate tools for:

- required documents
- customer-facing summary generation
- repair versus total-loss explanation
- denial-draft explanation

The MCP server should fill those smaller support functions. It should not
reimplement `recommendSettlement`, because that endpoint already exists in
`demo-offer-decision-api`.

## Decisions

- The backend MCP server is stateful.
- Session state is in memory for the demo.
- The server returns and validates `Mcp-Session-Id`.
- The first implementation exposes camelCase tool names only.
- The MCP server implements only gap-filling tools.
- Existing REST API capabilities stay in the REST APIs and are not duplicated.
- Small duplicated fixtures or deterministic rule tables are acceptable if they
  make the example faster to deliver.
- The existing `insurance-claim-mcp-v1.yaml` workflow should be replaced rather
  than copied into a second MCP workflow version.

## Proposed App

Create a new app in `light-example-rs`:

```text
apps/demo-insurance-claim-mcp-server/
  Cargo.toml
  src/main.rs
  config/
    client.yml
    portal-registry.yml
    server.yml
    startup.yml
    values.yml
```

Suggested service identity:

```yaml
server.serviceId: com.networknt.demo.insurance-claim-mcp-1.0.0
server.environment: demo
server.httpPort: 8087
server.enableHttp: true
server.enableRegistry: true
```

For local standalone development, `server.enableRegistry` can be overridden to
`false`. For the full demo, it should register with controller discovery so
`light-gateway` can resolve it by `serviceId`.

## Runtime Shape

The app should follow the same pattern as the REST examples:

```rust
#[derive(Clone, Default)]
struct InsuranceClaimMcpApp;

#[async_trait]
impl AxumApp for InsuranceClaimMcpApp {
    async fn router(&self, _context: ServerContext) -> Result<Router, RuntimeError> {
        Ok(build_router())
    }
}
```

The server should expose:

- `GET /health`
- `POST /mcp`
- `DELETE /mcp` for session cleanup

Use `LightRuntimeBuilder::new(AxumTransport::new(InsuranceClaimMcpApp))` and
the same config-dir environment override pattern used by the REST demos.

## MCP Protocol Scope

Keep the first server deliberately small:

- support JSON-RPC `initialize`
- support `notifications/initialized`
- support `tools/list`
- support `tools/call`
- issue an in-memory `Mcp-Session-Id` from `initialize`
- require later `tools/list` and `tools/call` requests to send a known
  `Mcp-Session-Id`
- support `DELETE /mcp` to remove the in-memory session
- return JSON-RPC errors for unknown methods, unknown tools, invalid arguments,
  and tool execution failures

Streaming can be deferred. The first version can return normal JSON responses
from `POST /mcp`.

## Tool Catalog

Implement only the tools that fill gaps in the current demo.

| Tool | Purpose |
| --- | --- |
| `evaluateCoverage` | Determine whether the incident date, policy status, and vehicle coverage allow the claim to continue. |
| `classifyLiability` | Classify liability as clear, unclear, contested, or external-party based on claim facts. |
| `scoreClaimRisk` | Produce risk level and SIU recommendation from prior claims, injury, drivable status, and claim facts. |
| `listRequiredDocuments` | Return required documents for repair, total-loss review, denial draft, or more-information path. |
| `generateCustomerSummary` | Produce a deterministic customer-facing summary from claim, coverage, triage, and settlement context. |

Do not implement these existing REST API capabilities in the MCP server:

- `getCustomerProfile`
- `getCustomerPreferences`
- `getCustomerPolicies`
- `getCoveredVehicle`
- `listPriorClaims`
- `triageClaim`
- `recommendSettlement`

The workflow should still show bounded agents. The MCP tools provide
deterministic coverage, liability, risk, document, and summary support that the
agents can reason over.

## Data Strategy

Because the MCP server does not duplicate REST endpoints, it does not need to
own the full customer, policy, vehicle, prior-claim, triage, or settlement data
sets. The workflow passes the REST API outputs into MCP gap tools as tool
arguments.

Small duplicated constants are acceptable for speed, for example:

- coverage rule thresholds
- liability classification labels
- risk scoring thresholds
- document templates
- customer summary text templates

Avoid creating a shared fixture crate unless duplication becomes hard to
maintain.

## Gateway Configuration

The full demo path should configure `light-gateway` with an `apiType: mcp`
backend target. The gateway remains the public MCP endpoint used by
`light-workflow`; the new server is the backend MCP implementation.

Conceptual target:

```yaml
mcp-router.enabled: true
mcp-router.path: /mcp
mcp-router.tools:
  - name: evaluateCoverage
    apiType: mcp
    serviceId: com.networknt.demo.insurance-claim-mcp-1.0.0
    envTag: demo
    path: /mcp
```

Repeat the tool entries for the gap-filling tools. Access-control rules and
response filtering should remain enforced at `light-gateway`.

## Implementation Phases

### Phase 1: App Skeleton

- add `apps/demo-insurance-claim-mcp-server`
- add workspace membership in `light-example-rs/Cargo.toml`
- implement `light-axum` startup, config-dir overrides, tracing, `/health`
- add config files and config-registry values
- add release/build wiring consistent with the two existing demo APIs

### Phase 2: Minimal MCP Protocol

- define JSON-RPC request, response, error, and MCP content/result structs
- implement in-memory session storage
- implement `initialize` with `Mcp-Session-Id`
- implement `notifications/initialized`
- implement `tools/list`
- implement `tools/call`
- validate `Mcp-Session-Id` on later requests
- implement `DELETE /mcp` session cleanup
- add request validation and JSON-RPC error mapping
- add unit tests for protocol errors

### Phase 3: Gap-Filling Tools

- implement `evaluateCoverage`
- implement `classifyLiability`
- implement `scoreClaimRisk`
- implement `listRequiredDocuments`
- implement `generateCustomerSummary`
- keep output fields aligned with the replacement workflow assertions
- add handler tests for one success and one failure path per tool group
- verify with direct `POST /mcp` `tools/list` and `tools/call`

### Phase 4: Gateway And Workflow Integration

- add demo `mcp-router.yml` entries that point to the MCP server by `serviceId`
- enable registry for the MCP server in the full demo environment
- verify `light-gateway` can initialize the backend MCP server
- replace `insurance-claim-mcp-v1.yaml` so it calls REST APIs for existing demo
  API capabilities and MCP tools for gap-filling capabilities
- run the replaced `insurance-claim-mcp-v1.yaml` flow through `light-gateway`
- keep the existing REST workflow unchanged for comparison

## Tests And Verification

Minimum verification:

```bash
cargo check -p demo-insurance-claim-mcp-server
cargo test -p demo-insurance-claim-mcp-server
```

Direct protocol checks:

```bash
curl -sS http://127.0.0.1:8087/health

curl -i -sS -X POST http://127.0.0.1:8087/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"demo","version":"1.0.0"},"capabilities":{}}}'

curl -sS -X POST http://127.0.0.1:8087/mcp \
  -H 'Content-Type: application/json' \
  -H 'Mcp-Session-Id: <session-id-from-initialize>' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

Gateway checks:

```bash
curl -k -sS -X POST https://localhost:8443/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

Workflow checks:

- start `insurance-claim-mcp-v1`
- confirm customer-context outputs are loaded through REST API calls
- confirm `triageClaim` and `recommendSettlement` are still REST API calls
- confirm `evaluateCoverage`, `classifyLiability`, `scoreClaimRisk`,
  `listRequiredDocuments`, and `generateCustomerSummary` run through MCP
- complete the adjuster and claimant human tasks
- verify the final workflow output is `CLAIM_APPROVED` for the happy path
