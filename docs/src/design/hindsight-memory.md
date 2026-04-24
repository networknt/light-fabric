# Hindsight Memory

**Hindsight Memory** is the core memory system for `light-rs`, designed to move beyond simple chat logs. Instead of just remembering what was said, the agent learns and forms mental models over time.

This design is strongly inspired by the paper [Hindsight is 20/20: Building Agent Memory that Retains, Recalls, and Reflects](https://arxiv.org/abs/2512.12818) and extends it with multi-tenant support.

---

## 1. Core Concepts

Hindsight memory organizes information into three distinct "Pathway" types:

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
| `agent_memory_bank_t` | The primary container. Defines personality and disposition (skepticism, empathy). |
| `agent_memory_doc_t` | Source documents (logs, files, transcripts) that provide the raw text for memory units. |
| `agent_memory_unit_t` | Sentence-level "atoms" of thought. Stores content, embeddings, and fact types (world, experience, etc.). |
| `agent_memory_entity_t` | Resolved Knowledge Graph nodes, optionally linked to platform users (`user_t`). |
| `agent_memory_unit_entity_t` | The join table linking individual memories to the entities they mention. |
| `agent_memory_entity_cooccur_t` | Association graph tracking concept relationships and co-occurrence counts. |
| `agent_memory_link_t` | Defines causal and semantic relationships between memories (causes, enables, etc.). |
| `agent_memory_directive_t`| "Hard rules" that override probabilistic learning. |
| `agent_memory_reflection_t`| Synthesized high-level insights generated during the "Reflect" phase. |
| `agent_session_history_t`| The live record of active conversations, linked to a specific bank for context. |

---

## 4. Privacy & Multi-Tenancy

Isolation is managed at the **Bank** level using three scoping tiers:

1.  **Global Host Bank** (`user_id` IS NULL, `agent_def_id` IS NULL):
    - Knowledge shared across all users and all agents within a specific `host_id`.
    - Ideal for organization-wide SOPs, common facts, and shared documentation.
2.  **Shared Agent Bank** (`user_id` IS NULL, `agent_def_id` IS NOT NULL):
    - Knowledge shared by all users interacting with a specific agent type.
    - Used for maintaining a consistent agent "Persona" or specialized domain expertise.
3.  **Private User Bank** (`user_id` IS NOT NULL):
    - Knowledge unique to a specific user.
    - Can be scoped further by `agent_def_id` to provide user-specific memory within a particular agent persona.
    - Used for personal preferences, private history, and individualized learning.

---

## 5. Implementation Guide

To implement a "Learning Agent," follow this sequence in your application logic:

1.  **Ingestion**: After every tool call or user interaction, call `retain` to update the bank.
2.  **Context Loading**: Before calling the LLM, call `recall` to fetch the most relevant 3-5 memories for the current prompt.
3.  **Scheduled Reflection**: Run the `reflect` operation during idle time to compress raw experiences into high-level mental models.
