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

## Summary: Which to Choose?

### Choose Light-Fabric if:
- You are building an **Enterprise AI Strategy** that requires unified governance across multiple teams.
- You need to manage the **entire lifecycle** of agents, not just their API calls.
- You require **advanced data privacy** (column/row masking) and **long-term memory** (Hindsight) out of the box.

### Choose AgentGateway if:
- You need a **lightweight proxy** to handle LLM provider failover and basic request normalization.
- You prefer to manage agent logic, memory, and governance entirely within your application code.
- You are looking for a simple tool to solve **immediate connectivity** needs without implementing a full platform layer.
