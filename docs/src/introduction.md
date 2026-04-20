# Introduction

Light-RS is a high-performance, asynchronous Rust framework designed for building scalable microservices and agentic AI systems.

## Why Light-RS?

- **Performance**: Built on top of `tokio` and `axum` for maximum throughput.
- **Agentic AI**: Specialized crates for LLM provider integration, tool calling, and workflow management.
- **Production Ready**: Includes robust features like retries, failover, and observability out of the box.

## Core Components

- **`light-rs/crates/model-provider`**: A unified interface for multiple LLM providers.
- **`light-rs/frameworks`**: Core infrastructure for services.
- **`light-rs/apps`**: Reference applications and microservices.
