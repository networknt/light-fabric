# Light-Fabric

Light-Fabric is a high-performance, unified platform for managing the lifecycle, governance, and orchestration of enterprise AI services, including agentic services, agents, tools, skills, memories, MCP servers, APIs, gateways, and workflows.

## Overview

Light-Fabric provides the runtime and control-plane foundation for managed agents and distributed AI components. It "weaves" together disparate services into a cohesive, secure, and observable ecosystem, ensuring enterprise-grade governance over autonomous agents and LLM-powered workflows.

## Key Features

- **Unified Control Plane**: A single point of truth for discovering, governing, and auditing agents and APIs via the Light-Portal.
- **Agentic Intelligence**: Built-in support for **Hindsight Memory** (biomimetic memory banks) and centralized agent skills.
- **Enterprise Security**: Fine-grained authorization and data filtering (masking) designed for corporate compliance.
- **High Performance**: Built with Rust, utilizing `tokio` and `axum` for maximum throughput and memory safety.
- **Production Ready**: Out-of-the-box support for retries, failover, and deep observability.

## Documentation

Full documentation, including architecture guides and implementation patterns, is available at:

**[https://networknt.github.io/light-fabric/](https://networknt.github.io/light-fabric/)**

## Core Components

- **`crates/model-provider`**: A unified interface for multiple LLM providers.
- **`frameworks`**: Core infrastructure for high-performance services.
- **`apps`**: Reference applications and enterprise microservices.

## Getting Started

To get started with the Light-Fabric, refer to the [Getting Started](docs/src/getting-started.md) guide in the documentation.

## License

This project is licensed under the Apache-2.0 License.
