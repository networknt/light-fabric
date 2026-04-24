# Comparison: Light-Fabric vs. AgentGateway

This document provides a high-level comparison between **Light-Fabric** and **AgentGateway** to help architects and engineering leaders choose the right foundation for their agentic workflows.

## Overview

While both systems aim to facilitate interaction with Large Language Models (LLMs), they operate at different layers of the AI stack and prioritize different architectural outcomes.

| Feature | Light-Fabric | AgentGateway |
| :--- | :--- | :--- |
| **Primary Philosophy** | **Agentic Fabric**: Unified Governance & Lifecycle | **Agentic Gateway**: High-performance Proxy |
| **Core Architecture** | Integrated Platform (Layer) | Standalone Gateway (Service) |
| **Target User** | Central IT / Platform Engineering | Application Developers / DevOps |
| **Lifecycle Management** | APIs, Agents, MCPs, and Gateways | Primarily LLM Request Routing |
| **Language** | Native Rust (Extreme Performance) | Rust / Go (Variable) |

---

## 1. Governance vs. Connectivity

### Light-Fabric (Governance)
Light-Fabric is designed as a **Single Control Plane**. It assumes that in an enterprise environment, "freedom without governance is chaos." It provides:
- **Centralized Registry**: Every agent, skill, and tool is registered and governed via the `light-portal`.
- **Fine-Grained Authorization**: Deep policy enforcement at the endpoint level, including row and column-level data masking.
- **Auditability**: A unified audit trail for all agentic interactions across the entire organization.

### AgentGateway (Connectivity)
AgentGateway typically focuses on the **North-South traffic** between an application and multiple LLM providers. Its primary strength is:
- **Simplified Routing**: Getting a request from Point A to Point B with retries and failover.
- **Provider Abstraction**: Normalizing different LLM APIs into a single interface.

---

## 2. Integrated Intelligence: Hindsight

One of the defining differences of the Light-Fabric is the deep integration of **Hindsight Memory**.

- **Light-Fabric**: Memory is not an "add-on." The platform provides native biomimetic memory banks (World Facts, Experiences, Mental Models) that are automatically managed and scoped (Global, Shared, Private) as part of the fabric.
- **AgentGateway**: Typically treats memory as external state. The application or a separate vector database must manage context before sending the request through the gateway.

---

## 3. Skill & Tool Management

### Centralized Skills (Fabric)
In Light-Fabric, skills (tools) are **first-class citizens**. They are registered, versioned, and governed centrally. An agent doesn't just "have" a tool; the Fabric *grants* the agent access to a skill based on its role and the current context.

### Standard Tooling (Gateway)
AgentGateway generally passes tool definitions through to the provider. The management of who can use which tool and how those tools are secured is usually left to the application logic.

---

## 4. Orchestration: Hybrid Agentic Workflows

### Light-Fabric (Integrated Orchestrator)
Light-Fabric treats orchestration as a foundational service. It implements a **Hybrid Model**:
- **Deterministic Process**: The overall business logic (e.g., insurance claim steps) is fixed and compliant.
- **Autonomous Tasks**: Individual steps within the process are delegated to agents.
- **Statefulness**: The Fabric manages long-running state across days or weeks, ensuring durability.

### AgentGateway (Stateless Proxy)
AgentGateway is primarily a stateless component. 
- **External Orchestration**: The workflow logic must reside in your application code or an external engine (like Temporal). 
- **Proxy Only**: It handles the communication but does not "understand" or manage the multi-step business process itself.

---

## 5. Security: The Rule Engine

### Light-Fabric (Integrated Governance)
Light-Fabric includes an integrated **YAML-based Rule Engine** (`light-rule`) designed for fine-grained authorization:
- **Data Filtering**: Automatically masks or filters response data (column/row level) based on policies.
- **Policy Enforcement**: Checks permissions *before* an agent executes a tool or accesses a memory unit.
- **Hot-Reloading**: Security rules can be updated in real-time without redeploying the platform.

### AgentGateway (Basic Middleware)
AgentGateway typically provides basic security features like API key validation or rate limiting.
- **Limited Filtering**: While it can intercept traffic, implementing complex, context-aware data masking usually requires writing custom middleware or handling it at the application level.

---

## 6. MCP Support: Gateway vs. Ecosystem

### Light-Fabric (Integrated Tooling)
Light-Fabric treats **Model Context Protocol (MCP)** as a primary source for agent tools.
- **Direct Integration**: Agents use the `mcp-client` to directly consume tools from MCP servers.
- **Registry Management**: MCP servers are registered in the `light-portal`, allowing for centralized discovery and governance.
- **Unified Security**: The same Fine-Grained Authorization rules apply to MCP tools as they do to native Rust tools.

### AgentGateway (Specialized MCP Proxy)
AgentGateway provides a highly specialized **MCP Gateway** layer.
- **Protocol Translation**: It excels at translating between different MCP transports (SSE, Streamable HTTP, etc.).
- **Exposing Servers**: Its primary role is to make MCP servers accessible to external applications through a normalized gateway interface.
- **Advanced Networking**: Includes features like stream merging and specialized MCP routing.

For a deep dive into the technical differences, see our **[Detailed MCP Feature Comparison](vs-agent-gateway-mcp.md)**.

---

## Summary: Which to Choose?

### Choose Light-Fabric if:
- You are building an **Enterprise AI Strategy** that requires unified governance, stateful workflows, and integrated security.
- You need to manage the **entire lifecycle** of agents and the business processes they participate in.
- You require **advanced data privacy** (masking) and **long-term memory** (Hindsight) as native platform features.

### Choose AgentGateway if:
- You need a **lightweight proxy** to handle LLM provider failover and basic request normalization.
- You prefer to manage agent logic, workflows, memory, and security entirely within your external application stack.
- You are looking for a simple tool to solve **immediate connectivity** needs without implementing a comprehensive platform layer.
