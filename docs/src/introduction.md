# Light-Fabric

Light-Fabric is a high-performance, unified platform for managing the lifecycle, governance, and orchestration of enterprise AI services including agentic services, agents, tools, skills, memories, MCP servers, APIs, gateways and workflows.

## Why Light-Fabric?

We chose the name **Light-Fabric** because it embodies the "Unified Governance" required for enterprise-grade AI:

- **Unified Control Plane**: Light-Fabric provides a single point of truth for discovering, governing, and auditing agents, MCP servers, and APIs via the `light-portal`.
- **Enterprise Governance**: It prioritizes security and policy enforcement (such as fine-grained authorization) over pure decentralized autonomy, making it safe for corporate environments.
- **Integrated Ecosystem**: It "weaves" together distributed components—from memory units (Hindsight) to centralized skills—into a cohesive, observable system.
- **Durable Identity**: The name emphasizes the platform's role as the infrastructure foundation, remaining relevant regardless of the underlying implementation details.

## Technical Advantages

By building Light-Fabric on a Rust foundation, we achieve:

- **Performance**: Built on top of `tokio` and `axum` for maximum throughput and memory safety.
- **Native Intelligence**: Specialized crates for Hindsight memory, tool calling, and workflow orchestration.
- **Production Ready**: Includes robust features like retries, failover, and observability out of the box.

## Core Components

The Light-Fabric is composed of modular crates, infrastructure frameworks, and reference applications:

### Crates
- **`crates/model-provider`**: A unified interface for multiple LLM providers (Ollama, etc.).
- **`crates/hindsight-client`**: Client for the Hindsight biomimetic memory system.
- **`crates/mcp-client`**: Implementation of the Model Context Protocol (MCP) for tool discovery and execution.
- **`crates/portal-registry`**: Integration with the Light-Portal for service registration and discovery.
- **`crates/light-runtime`**: Core runtime foundation for building agentic and microservice components.
- **`crates/light-rule`**: High-performance [rule engine](https://github.com/agentic-workflow/rule-specification) for fine-grained authorization and data filtering.
- **`crates/workflow-core` & `workflow-builder`**: Core engine and builder for complex agentic workflows.
- **`crates/config-loader`**: Flexible configuration management for enterprise environments.
- **`crates/asymmetric-decryptor` & `symmetric-decryptor`**: Security utilities for sensitive data handling.

### Frameworks
- **`frameworks/light-axum`**: A specialized microservice & agentic framework built on top of the Axum web ecosystem.
- **`frameworks/light-pingora`**: High-performance proxy and gateway framework built on top of Cloudflare's Pingora.

### Applications
- **`apps/light-agent`**: A managed AI agent capable of using tools, accessing memory, and executing complex tasks.
- **`apps/light-gateway`**: An enterprise-grade gateway for securing and governing API and agent traffic.
- **`apps/light-workflow`**: A service for orchestrating and executing long-running [agentic workflows](https://github.com/agentic-workflow/workflow-specification).
