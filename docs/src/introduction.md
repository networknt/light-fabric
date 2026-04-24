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

- **`light-fabric/crates/model-provider`**: A unified interface for multiple LLM providers.
- **`light-fabric/frameworks`**: Core infrastructure for services.
- **`light-fabric/apps`**: Reference applications and microservices.
