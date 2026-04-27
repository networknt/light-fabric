# Why Light-Fabric Already Covers the MCP Gateway — No Second Gateway Required

This document addresses a recommendation (produced by Grok AI) suggesting that an enterprise should deploy the open-source **AgentGateway** as a dedicated MCP layer alongside an existing API platform. After performing a **side-by-side source code analysis** of both projects (see [vs-agentgateway.md](vs-agentgateway.md) and [vs-agent-gateway-mcp.md](vs-agent-gateway-mcp.md)), we present the findings below.

---

## 1. The Recommendation Was Generated Without Knowledge of Light-Fabric

The Grok-produced analysis operates under a critical blind spot: **it has no knowledge of Light-Fabric (Rust-based, open-sourced to customers) or its capabilities**. The recommendation frames the choice as "keep your existing REST platform + add AgentGateway for MCP," because Grok only knows about publicly documented open-source projects. It does not account for the fact that:

- **Light-Fabric is already in production** and serving agentic workloads today.
- Every feature listed in the recommendation — MCP federation, tool discovery, protocol translation, security, and observability — has already been **built, demonstrated, and validated** with the project team.
- The comparison is therefore not between "a REST framework" and "an MCP gateway." It is between **two systems that both provide MCP gateway capabilities**, where one (Light-Fabric/Light-Gateway) is already deployed and battle-tested in our environment.

---

## 2. Source Code Analysis: Light-Fabric Already Does What AgentGateway Does

We conducted a detailed, code-level comparison of both projects. The full results are documented in our [High-Level Comparison](vs-agentgateway.md) and [Detailed MCP Feature Comparison](vs-agent-gateway-mcp.md). The key findings are summarized below.

### 2.1 MCP Protocol Support

| Capability | Light-Fabric | AgentGateway |
| :--- | :--- | :--- |
| **Transports** | SSE, Streamable HTTP, WebSocket | SSE, Streamable HTTP, WebSocket |
| **Tool Discovery** | Auto-discovery via `tools/list` sync | Manual K8s CRD configuration |
| **Protocol Translation** | Native REST/RPC → MCP transformation | Manual wrappers required |
| **Stream Handling** | Supported | Supported (mergestream) |

Both projects support the same MCP transports. Light-Fabric goes further with **automatic tool discovery** and **native protocol transformation** from existing REST/RPC APIs — exactly the "OpenAPI-to-MCP mapping" that the Grok recommendation credits to AgentGateway, except Light-Fabric does it without requiring a separate component.

### 2.2 Security & Authorization

| Capability | Light-Fabric | AgentGateway |
| :--- | :--- | :--- |
| **Authentication** | JWT (end-to-end propagation) | JWT, Keycloak, OIDC, Passthrough |
| **Authorization** | Role, Group, Position, Attribute-based (ABAC/PBAC) | CEL-based policies |
| **Data Privacy** | Row/Column-level masking | Allow/Deny access control |
| **Rule Engine** | Integrated YAML-based, hot-reloadable | Basic middleware |

The Grok recommendation highlights "tool-level RBAC" and "MCP-compliant OAuth 2.1" as AgentGateway strengths. Our code analysis shows that Light-Fabric's authorization model is **significantly deeper** — it supports corporate-hierarchy-aware policies and content-level data masking that AgentGateway simply does not implement.

### 2.3 Lifecycle & Operations

| Capability | Light-Fabric | AgentGateway |
| :--- | :--- | :--- |
| **Onboarding** | Portal-driven, auto-discovery | K8s manifest-driven, manual |
| **Hot-Reloading** | Native (Config Server + Control Plane) | Infrastructure-dependent (Istio/xDS) |
| **Observability** | OTEL + integrated Hindsight Memory | OTEL + OpenInference |
| **Orchestration** | Integrated hybrid workflows (deterministic + autonomous) | None (stateless proxy) |

Light-Fabric manages the **entire lifecycle** — from tool registration through governance to runtime orchestration — while AgentGateway only handles the proxy layer.

---

## 3. Two Gateways Is Overkill

The Grok recommendation frames the architecture as a "clean separation of concerns." In practice, deploying both Light-Fabric and AgentGateway creates **redundant infrastructure** with real costs:

### Duplicated Capabilities

Both systems would be performing the same core functions:
- Receiving MCP requests from agents
- Translating tool calls to backend HTTP requests
- Enforcing security policies on tool access
- Providing observability for agentic traffic

Running two gateways that do the same thing is not "separation of concerns" — it is **duplication of concerns**. Every MCP request would traverse two proxy layers instead of one, adding latency and operational complexity for zero additional capability.

### Operational Burden

- **Two deployment pipelines** to maintain on EKS
- **Two sets of security policies** to keep in sync
- **Two configuration surfaces** (K8s CRDs for AgentGateway vs. Portal for Light-Fabric)
- **Two failure domains** to monitor and troubleshoot
- **Two upgrade cycles** to coordinate

### The "No Code Changes" Claim Is Misleading

The Grok recommendation states AgentGateway requires "no code changes." This is true only if you ignore the work required to:
- Write and maintain Kubernetes Custom Resources for every MCP backend
- Build manual wrappers for non-MCP services (Light-Fabric does this natively)
- Implement application-level logic for everything AgentGateway doesn't cover (stateful workflows, data masking, memory management)

Light-Fabric also requires no code changes to existing backend services — and it provides the governance layer out of the box.

---

## 4. Addressing the "Rust Performance" Argument

The recommendation claims AgentGateway has a "performance edge" due to its Rust data plane. This argument does not hold:

- **Light-Fabric's AI Gateway currently runs on the high-performance Java-based light-gateway, and a new Rust-based AI Gateway is also under way**, built on the Pingora framework (Cloudflare's production proxy engine). Even the existing Java gateway delivers exceptional throughput, and the Rust gateway will remove the JVM from the critical path entirely.
- Both systems benefit from Rust's zero-cost abstractions, memory safety, and lack of garbage collection pauses.
- The performance comparison between the two Rust implementations would be marginal and workload-dependent — not a differentiator.

---

## 5. Addressing the "Custom Development" Concern

The recommendation warns against "implementing MCP directly" because it "involves significant custom development." This concern does not apply:

- Light-Fabric's MCP support is **not custom development** — it is a fully implemented, production-ready feature of the platform.
- The MCP client, gateway routing, tool registry, and security integration are all **existing, tested components**, not a backlog of work to be done.
- The project team has already seen these features demonstrated end-to-end.

---

## 6. Summary

| Concern from Grok Recommendation | Reality |
| :--- | :--- |
| "Light4j is a REST framework, not an AI proxy" | Light-Fabric is a full agentic platform with an AI Gateway already in production |
| "AgentGateway provides MCP federation and tool discovery" | Light-Fabric provides the same capabilities with deeper governance |
| "Rust performance advantage over JVM" | Light-Fabric's Java gateway is already very fast, and a Rust (Pingora-based) gateway is coming |
| "Clean separation of concerns" | Two gateways doing the same thing is duplication, not separation |
| "No code changes required" | True for both — but AgentGateway requires extensive K8s manifest management |
| "Custom MCP implementation is risky" | Light-Fabric's MCP support is already built, tested, and in production |

### Conclusion

The Grok-generated recommendation is well-structured but fundamentally flawed because **it was produced without knowledge of Light-Fabric's capabilities**. When evaluated against the actual source code and production state of both systems, the case for adding AgentGateway collapses:

- Light-Fabric already provides every MCP gateway capability that AgentGateway offers.
- Light-Fabric goes significantly further with integrated governance, data privacy, memory, and orchestration.
- Adding a second gateway introduces operational complexity and latency with no net-new capability.

The pragmatic, low-risk path is to continue with the platform that is **already built, already in production, and already proven** to the team.
