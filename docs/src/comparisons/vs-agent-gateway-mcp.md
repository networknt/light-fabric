# Detailed Comparison: MCP Gateway Features

This document provides a technical deep dive into the **Model Context Protocol (MCP)** implementations in **Light-Fabric** and **AgentGateway**.

## Feature Matrix

| Feature | Light-Fabric | AgentGateway |
| :--- | :--- | :--- |
| **Primary Role** | **Provider/Gateway/Portal**: Exposes MCP/API Servers. | **Provider/Gateway**: Exposes MCP servers. |
| **Onboarding** | **Auto-Discovery**: Automatic `tools/list` sync. | **Manual**: K8s CRD/Manifest configuration. |
| **Data Privacy** | **Deep**: Row/Column level masking. | **Basic**: Allow/Deny access control. |
| **Transports** | SSE, Streamable HTTP, WebSocket | SSE, Streamable HTTP, WebSocket |
| **Legacy Integration**| **Native**: REST/RPC to MCP transformation. | **External**: Manual wrappers required. |
| **Authorization** | **Managed**: Roles, Groups, Positions, Attributes. | **Infrastructure**: CEL-based policies. |
| **Hot-Reloading** | **Native**: Integrated Control Plane & Registry. | **Infrastructure**: Istio/xDS sync. |
| **Authentication** | JWT (End-to-End Propagation) | JWT, Keycloak, OIDC, Passthrough |
| **Observability** | Distributed Tracing (OTEL) and Integrated **Hindsight Memory** | Distributed Tracing (OTEL) |

---

## 1. Architectural Intent

### AgentGateway: The Network Proxy Layer
AgentGateway is designed as a **high-availability proxy** for MCP servers. Its primary focus is the **North-South traffic** between an application and multiple MCP backends.
- **Multiplexing**: Optimized for merging multiple MCP backends into a single upstream connection (`mergestream.rs`).
- **Protocol Translation**: Excels at translating between SSE, Streamable HTTP, and WebSocket transports.
- **Infrastructure Focus**: Operates as a Kubernetes-native component managed via manifests and standard networking policies.

### Light-Fabric: The Managed Enterprise Platform
Light-Fabric provides a **Unified Governance Fabric** that treats AI agents and MCP tools as part of the broader enterprise API ecosystem.
- **Unified Gateway**: The AI Gateway (Rust/Pingora-based) serves as a single entry point for UI, Agents, and Tools, supporting both MCP and traditional REST/RPC APIs.
- **Centralized Portal**: Uses the **Light-Portal** as a control plane for onboarding (auto-discovery), configuration (hot-reloading), and security management.
- **Governed Intelligence**: Integrates the gateway directly with **Hindsight Memory** and the **Fine-Grained Rule Engine**, ensuring that every tool call is governed by corporate compliance rules (e.g., row/column masking).
- **End-to-End Security**: Maintains a single JWT-based identity from the user's chat interface all the way to the underlying MCP or API endpoint.

---

## 2. Security & Authorization

### AgentGateway: Infrastructure-Aware RBAC
AgentGateway uses **Common Expression Language (CEL)** for its authorization policies.
- **Capabilities**: High-speed, network-level blocking based on JWT claims and request headers.
- **Limitation**: Lacks native support for content-aware data masking or organizational hierarchy logic.

### Light-Fabric: Content-Aware Managed Auth
Light-Fabric provides a mature **Fine-Grained Authorization** layer:
- **Managed ABAC/PBAC**: Supports Role, Group, **Corporate Position (Hierarchy)**, and Attribute-based protection.
- **Data Privacy**: Supports native **Row and Column filtering** (data masking), ensuring agents only see data they are authorized to process.
- **End-to-End JWT**: The same JWT token is propagated from the UI through the Agent to the AI Gateway and MCP tool.

---

## 3. Lifecycle & Tool Onboarding

### AgentGateway: Configuration-Driven
Onboarding tools in AgentGateway is an infrastructure task:
- **Manual Mapping**: Requires defining Kubernetes Custom Resources (`HTTPRoute`, `Backend`) to map MCP servers to the gateway.
- **Scope**: Primarily focused on exposing existing MCP servers.

### Light-Fabric: Registry-Driven
Light-Fabric provides a "Zero-Effort" onboarding experience via **Light-Portal**:
- **Auto-Discovery**: Registering an MCP API triggers an automatic `tools/list` call to populate the registry.
- **Protocol Transformation**: Automatically transforms existing **OpenAPI/REST and RPC** services into MCP tools without requiring wrappers.
- **Centralized Governance**: All tools (Native, REST, MCP) are managed in a single unified registry.

---

## 4. Control Plane & Configuration

### AgentGateway: Kubernetes-Native
- **Orchestration**: Managed via the Istio/xDS control plane.
- **Updates**: Configuration changes are applied via Kubernetes manifests (YAML).

### Light-Fabric: Portal-Managed
- **Hot-Reloading**: Uses a dedicated **Config Server** and **Control Plane** to update gateway and agent configurations in real-time without restarts.
- **Enterprise Management**: Business-centric UI for managing tool visibility, agent permissions, and security policies.


## 5. Conclusion

- **Use AgentGateway** if you are an infrastructure provider who needs to expose MCP-based tools to multiple external applications securely and reliably.
- **Use Light-Fabric** if you are building intelligent agents that need to use those tools to solve complex business problems within a governed framework.
