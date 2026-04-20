# Advanced Agent Memory (Hindsight Integration)

Hindsight™ is the core memory system for `light-rs`, designed to move beyond simple chat logs into **Biomimetic Memory**. Instead of just remembering what was said, the agent learns and forms mental models over time.

---

## 1. Core Concepts

Hindsight organizes information into three distinct "Pathway" types:

1.  **World Facts**: Objective truths about the environment (e.g., "The production server is in US-East-1").
2.  **Experiences**: The agent's own history of actions and results (e.g., "I tried to deploy to US-East-1 and it failed due to a timeout").
3.  **Mental Models**: Synthesized understandings formed by reflecting on facts and experiences (e.g., "Deployments to US-East-1 are unstable during peak hours").

---

## 2. The Three Operations

Interaction with the memory system is standardized into three primary operations:

### Retain (Storage)
The `retain` operation ingests information. Behind the scenes, the system:
- Extracts entities and relationships.
- Normalizes time and temporal data.
- Stores the data in `agent_memory_unit_t`.

### Recall (Retrieval)
The `recall` operation retrieves relevant context using a hybrid strategy:
- **Semantic**: Vector similarity using the `hnsw` index.
- **Graph**: Following links in `agent_memory_link_t` (causes, enables, prevents).
- **Temporal**: Time-series filtering.

### Reflect (Synthesis)
The `reflect` operation performs "deep thinking." It analyzes existing memories to generate new insights, which are stored in `agent_memory_reflection_t`.

---

## 3. Database Architecture

The Hindsight system is fully integrated into the portal's multi-tenant schema:

| Table Name | Description |
| :--- | :--- |
| `agent_memory_bank_t` | The primary container. Defines disposition (skepticism, empathy). |
| `agent_memory_unit_t` | Sentence-level memories with vector embeddings. |
| `agent_memory_entity_t` | Knowledge Graph nodes, linked to system users (`user_t`). |
| `agent_memory_entity_cooccur_t` | Tracks how often entities appear together for associative recall. |
| `agent_memory_link_t` | Defines causal and semantic relationships between memories. |
| `agent_memory_directive_t`| "Hard rules" that override probabilistic learning. |

---

## 4. Privacy & Multi-Tenancy

Isolation is managed at the **Bank** level using three scoping tiers:

1.  **Global Host Bank** (`user_id` is NULL, `agent_def_id` is NULL):
    - Knowledge shared across all users and all agents in the organization.
    - Used for company-wide documentation and SOPs.
2.  **Shared Agent Bank** (`user_id` is NULL, `agent_def_id` is SET):
    - Knowledge shared by all users interacting with a specific agent type.
    - Used for agent "Personas" or specialized domain knowledge.
3.  **Private User Bank** (`user_id` is SET):
    - Knowledge unique to a specific user.
    - Used for personal preferences, private history, and individualized learning.

---

## 5. Implementation Guide

To implement a "Learning Agent," follow this sequence in your application logic:

1.  **Ingestion**: After every tool call or user interaction, call `retain` to update the bank.
2.  **Context Loading**: Before calling the LLM, call `recall` to fetch the most relevant 3-5 memories for the current prompt.
3.  **Scheduled Reflection**: Run the `reflect` operation during idle time to compress raw experiences into high-level mental models.
