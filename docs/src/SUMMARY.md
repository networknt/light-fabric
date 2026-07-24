# Summary

[Introduction](introduction.md)

---

[Getting Started](getting-started.md)

# Feature

- [Model Providers](features/providers.md)

# Design

- [Agentic Workflow](design/agentic-workflow.md)
- [Workflow Client Architecture](design/workflow-client-architecture.md)
- [LightAPI Description](design/lightapi-description.md)
- [Light-Rule](design/light-rule.md)
- [CEL Rule Conditions](design/cel-rule.md)
- [Debugging CEL Rules](design/debug-cel-rule.md)
- [Access Control Handler](design/access-control.md)
- [Centralized Skills](design/centralized-agent-skills.md)
- [Skill Workflow Orchestration](design/skill-workflow-orchestration.md)
- [Hindsight Memory](design/hindsight-memory.md)
- [Fine-Grained Authorization](design/fine-grained-authorization.md)
- [Agent Engine Pattern](design/agent-engine-pattern.md)
- [Light-Agent Execution](design/light-agent-execution.md)
- [Database Design](design/database-design.md)
- [Light-Deployer](design/light-deployer.md)
- [Module Registry](design/module-registry.md)
- [Module Hot Reload](design/module-hot-reload.md)
- [Controller Registry Client](design/controller-registry.md)
- [Cache Control Plane](design/cache-control-plane.md)
- [Client Configuration](design/client-configuration.md)
- [Embedded Configuration Templates](design/embedded-config-templates.md)
- [Handler Chain](design/handler-chain.md)
- [MCP Router](design/mcp-router.md)
- [MCP 2026-07-28 Dual-Profile Gateway](design/mcp-2026-07-28.md)
- [WebSocket Router](design/websocket-router.md)
- [Stateless Auth Handler](design/stateless-auth.md)
- [MSAL Exchange Handler](design/msal-exchange.md)
- [MSAL Auth Handler](design/msal-auth.md)
- [Unified Security Handler](design/unified-security.md)
- [PII Tokenization](design/pii-tokenization.md)
- [Token Handler](design/token-handler.md)
- [Service Discovery](design/service-discovery.md)
- [Tracing](design/tracing.md)
- [Release Workflow](design/release-workflow.md)
- [Light-Workflow Runner](design/light-workflow-runner.md)

# Implementation

- [Light-Axum](implementation/light-axum.md)
  - [Insurance Claim MCP Server](implementation/light-axum/insurance-claim-mcp-server.md)

# Crate

- [Asymmetric Decryptor](crate/asymmetric-decryptor.md)
- [Config Loader](crate/config-loader.md)
- [Hindsight Client](crate/hindsight-client.md)
- [Light Rule](crate/light-rule.md)
- [Light Runtime](crate/light-runtime.md)
- [MCP Client](crate/mcp-client.md)
- [Model Provider](crate/model-provider.md)
- [Portal Registry](crate/portal-registry.md)
- [Symmetric Decryptor](crate/symmetric-decryptor.md)
- [Workflow Builder](crate/workflow-builder.md)
- [Workflow Core](crate/workflow-core.md)

# Framework
- [Light-Axum](framework/light-axum.md)
  - [Rest API](framework/light-axum/rest-api.md)
  - [IPv6 Support](framework/light-axum/ipv6-support.md)

- [Light-Pingora](framework/light-pingora.md)
  - [MSAL Exchange](framework/light-pingora/msal-exchange.md)
# Product

- [Light-Agent](product/light-agent.md)
  - [Deploy Native](product/light-agent/deploy-native.md)
  - [Deploy Kubernetes](product/light-agent/deploy-kubernetes.md)
- [Light-Deployer](product/light-deployer.md)
  - [Build Local](product/light-deployer/build-local.md)
  - [Prepare Config](product/light-deployer/prepare-config.md)
  - [Run Standalone](product/light-deployer/run-standalone.md)
  - [Run Kubernetes](product/light-deployer/run-kubernetes.md)
- [Light-Gateway](product/light-gateway.md)
  - [Light Rule](product/light-gateway/light-rule.md)
  - [LLM Gateway](product/light-gateway/llm-gateway.md)
    - [Phase 0 ADR: Public Compatibility](adr/llm-gateway/0001-public-compatibility.md)
    - [Phase 0 ADR: Application Body Contract](adr/llm-gateway/0002-application-body-contract.md)
    - [Phase 0 ADR: Runtime Snapshot](adr/llm-gateway/0003-runtime-snapshot.md)
    - [Phase 0 ADR: Publication Transport](adr/llm-gateway/0004-publication-transport.md)
    - [Phase 0 ADR: Secret Materialization](adr/llm-gateway/0005-secret-materialization.md)
    - [Phase 0 ADR: Accounting Circuit Replay](adr/llm-gateway/0006-accounting-circuit-replay.md)
    - [Phase 0 ADR: Audit Durability](adr/llm-gateway/0007-audit-durability.md)
  - [Deploy Native](product/light-gateway/deploy-native.md)
  - [Deploy Kubernetes](product/light-gateway/deploy-kubernetes.md)
  - [Kubernetes Gateway API](product/light-gateway/k8s-gateway-api.md)
  - [IPv6 Support](product/light-gateway/ipv6-support.md)
  - [Call MCP Server with Curl](product/light-gateway/curl-mcp.md)
  - [MCP Tools Access Control](product/light-gateway/mcp-tools-access-control.md)
  - [MCP Tools List Access Control](product/light-gateway/mcp-tools-list-access-control.md)
  - [MCP Tool Metadata Usage](product/light-gateway/mcp-tool-metadata-usage.md)
- [Light-Workflow](product/light-workflow.md)
  - [Start Workflow](product/light-workflow/start-workflow.md)
  - [Native Agent Call](product/light-workflow/native-agent-call.md)
    - [Execution Backends And Sandbox Execution](product/light-workflow/sandbox-execution.md)
  - [Insurance Claim Agentic Workflow](product/light-workflow/insurance-claim-agentic-workflow.md)
  - [Light Portal Setup](product/light-workflow/light-portal-setup.md)

# Comparisons

- [vs. AgentGateway](comparisons/vs-agentgateway.md)
  - [Detailed MCP Comparison](comparisons/vs-agent-gateway-mcp.md)
  - [Email Answer](comparisons/email-answer.md)
