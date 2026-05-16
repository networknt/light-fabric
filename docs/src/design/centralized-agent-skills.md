# Design Document: Centralized Agentic Skill Registry

**Subject:** Transitioning from File-Based Markdown Skills to a Database-Backed Skill Registry

---

## 1. Executive Summary
Currently, most AI agent frameworks rely on localized Markdown (`.md`) files to define agent "skills." While Markdown is highly LLM-native and human-readable, it creates significant bottlenecks at an enterprise scale regarding strict typing, API integration, and context window limits. 

This document proposes transitioning to an **Agentic Control Plane (Centralized Skill Registry)** backed by a database. By decoupling skill *metadata*, *schemas*, and *instructions*, and by utilizing dynamic routing, we will achieve hierarchical structuring, strict schema enforcement, and progressive disclosure of tools to agents.

---

## 2. Problem Statement
Managing agent skills as flat Markdown files introduces several scaling challenges:
1. **Lack of Strict Typing:** Markdown cannot enforce data types (e.g., ensuring a parameter is an integer vs. string), leading to hallucinated or malformed tool inputs.
2. **Context Window Exhaustion:** Loading dozens or hundreds of skill definitions at startup overwhelms the LLM context window, increasing latency, token costs, and tool-misuse.
3. **Static Deployments:** Updating a skill or changing access permissions requires a full application redeploy.
4. **Poor Discoverability:** Flat file structures offer no native mechanism for progressive disclosure or tool search.

---

## 3. Data Models & Formats
To solve the limitations of purely text-based skills, we will adopt a hybrid, structured format stored within a database (e.g., PostgreSQL/MongoDB). The architecture uses the right format for the right job:

*   **JSON Schema:** Used strictly for defining parameters, inputs, and tool shapes. Natively supported by OpenAI/Anthropic/Google tool-calling APIs.
*   **LightAPI Description (YAML/JSON):** Used to map endpoint-level API capabilities to skills across REST, JSON-RPC, gRPC, and MCP.
*   **OpenAPI / OpenRPC / Protobuf:** Referenced by LightAPI where protocol-native specifications already exist.
*   **Executable Code (Python/JS) / URI:** Stores the actual execution logic or the endpoint reference.
*   **Markdown:** Retained *only* for the `instructions` or `prompt` fields, as LLMs excel at parsing markdown headers and lists for constraints and persona instructions.

LightAPI is the preferred source format for API-backed skills because it describes endpoint identity, protocol invocation, input schema, request mapping, result shape, examples, and behavior notes in one agent-oriented document. See [LightAPI Description Design](lightapi-description.md) for the endpoint description model.

YAML and JSON are the external skill document formats. In the portal database,
they should not replace the Markdown instruction field. The normalized model is
structured columns and relationships for identity, versioning, taxonomy, tools,
and execution metadata, plus `content_markdown` for the LLM-facing instruction
body. If the portal later needs to persist a full structured skill document,
add a nullable JSONB skill-spec column beside `content_markdown` and normalize
YAML imports to JSON.

### 3.1 Proposed Database Schema Structure
Light Portal stores skills in structured catalog tables. Below is a representation of the skill payload:

```json
{
  "skill_id": "sk_finance_001",
  "name": "generate_financial_report",
  "version": "1.2.0",
  "tags": ["finance", "reporting"],
  "tool_schema": {
    "type": "function",
    "function": {
      "name": "generate_financial_report",
      "description": "Generates a Q3 report based on ticker symbol.",
      "parameters": {
        "type": "object",
        "properties": {
          "ticker": {"type": "string", "description": "The stock ticker"}
        },
        "required": ["ticker"]
      },
      "response_schema": {
        "type": "object",
        "properties": {
          "report_url": {"type": "string"},
          "status": {"type": "string"}
        }
      }
    }
  },
  "execution": {
    "type": "rest_api",
    "endpoint_id": "ep_finance_report_001",
    "endpoint": "https://internal-api.company.com/v1/finance/report",
    "method": "POST"
  },
  "instructions": "## Role\nYou are a financial analyst.\n## Constraints\n- Never hallucinate financial data.\n- Always return exact numbers."
}
```

---

## 4. Hierarchical Structure & Progressive Disclosure
Dumping 500 JSON schemas into an LLM's context window will cause system failure. The Centralized Controller will act as a mediator, enforcing **hierarchy** and **progressive disclosure** (giving the agent only the schemas it needs, exactly when it needs them).

### 4.1 Implementing Hierarchy & Tagging
Because JSON Schema does not have built-in folders, hierarchy and categorization are enforced via the platform's global entity management system:
1.  **Namespacing:** Tool names follow a strict convention: `[domain]_[subdomain]_[action]` (e.g., `aws_rds_provision`).
2.  **Tags & Categories:** Instead of hardcoded columns, the registry utilizes the `entity_tag_t` and `entity_category_t` tables (with `entity_type = 'skill'`). This allows for unlimited flat tagging and deep hierarchical folder structures that are consistent across the entire portal.
3.  **Discovery API:** Portal-query filters by these tags/categories to scoped skill sets for specific agent personas. Agents cache the effective catalog locally and reload it when runtime cache-management invalidation is triggered.

### 4.2 Progressive Disclosure Patterns
Agents should not load every executable tool into the LLM context. Instead, they should load their assigned skill/tool catalog from the portal API, cache it locally, and use one of the following progressive disclosure patterns:

Phase 5 starts with the Rust `light-agent`. The agent loads
`genai-query/getEffectiveAgentCatalog`, keeps a local cache keyed by
`hostId + agentDefId + serviceId + envTag`, ranks cached skill/tool entries with
keyword and routing-field matching, and intersects the selected tool names with
the live gateway `tools/list` result before giving schemas to the model.
Execution remains gateway `tools/call`.

#### Pattern A: Meta-Tools (Dynamic Injection)
The agent is booted with only two "meta-tools" designed for discovery.
1.  Local catalog search: Agent searches its cached assigned skills. The cache contains lightweight summaries and mapped tool names.
2.  Schema loading: Once the agent identifies the correct tool, it loads the schema from the local catalog cache or refreshes the cache from portal-query.

#### Pattern B: Semantic Tool RAG (Zero-Shot Discovery)
For highly complex systems with thousands of skills:
1.  Tool descriptions are embedded into a Vector Database (e.g., `pgvector`).
2.  When the user prompts the system (e.g., "Reset my AWS password"), portal-query or the agent's local cache performs semantic search and retrieves the Top-3 most relevant JSON Schemas.
3.  The agent boots with *only* those 3 tools in its context. 

#### Pattern C: Multi-Agent Orchestration (Supervisor / Worker)
Hierarchy is mapped to agent teams.
1.  A **Supervisor Agent** holds routing tools (e.g., `delegate_to_finance`, `delegate_to_devops`).
2.  When `delegate_to_devops` is triggered, the supervisor routes to a **DevOps Worker Agent**, loading only the specific DevOps JSON schemas into its context.

---

## 5. Example Flow: Dynamic Loading in Action

**User:** *"I need to provision a new database for the marketing team."*

1.  **Turn 1: Discovery**
    *   *Agent Context:* Has a local cache of assigned skill summaries.
    *   *Agent Action:* Searches the local cache for `provision database`.
2.  **Turn 2: High-Level Awareness**
    *   *Local Cache Result:* Returns token-efficient summaries from the portal catalog:
        `[{"name": "aws_rds_provision", "description": "Creates AWS RDS DB"}, {"name": "mongo_atlas_create", "description": "Creates Mongo cluster"}]`
    *   *Agent Action:* Decides AWS is needed and loads the cached schema for `aws_rds_provision`.
3.  **Turn 3: Strict Execution**
    *   *Agent Catalog:* Provides the full JSON schema (requiring `instance_type`, `storage_gb`).
    *   *Agent Action:* Understands parameters and safely executes `aws_rds_provision` through the gateway `tools/call` path.

---

## 6. Operational Benefits & Security
By centralizing skills in a database, the platform gains enterprise-grade operational capabilities:
*   **Dynamic Updates:** API endpoints, instructions, and schemas can be updated in the database without restarting agents.
*   **Permission-Aware Discovery (RBAC):** By linking skills to LightAPI endpoint descriptions and `api_endpoint_t`, portal-query can limit catalog disclosure to the current agent or tenant, while runtime gateway policy still authorizes execution.
*   **A/B Testing:** Portal catalog metadata can route 50% of an agent's requests to `skill_v1` and 50% to `skill_v2` to measure prompt/tool efficacy.
*   **Audit Logging:** Catalog disclosure and gateway execution can be logged separately, preserving a compliance trail without moving tool execution into the registry.
*   **Distilled Memory RAG:** Following the "Hindsight" pattern, raw conversation history (`agent_session_history_t`) is separated from RAG-optimized memory (`session_memory_t`). This prevents the "noisy context" problem while maintaining a perfect audit trail.

## 7. LightAPI As Skill Source

API-backed skills should be generated from endpoint-level LightAPI descriptions whenever possible.

The skill registry should store skill metadata, access control, grouping, and agent-facing instructions. The LightAPI description should remain the source of truth for endpoint invocation and verification details.

Recommended flow:

1. Light-Portal creates or imports endpoint-level LightAPI descriptions.
2. API owners enrich endpoint descriptions with examples, behavior notes, result cases, and visibility.
3. Approved endpoint descriptions are published as agent skills.
4. The agent loads assigned skill summaries from portal-query and caches them locally.
5. When the agent selects a skill, it loads the relevant LightAPI disclosure level from the local cache or refreshes from portal-query.
6. Execution goes through the gateway `tools/call` path, preserving runtime policy and downstream authorization.

This avoids manually duplicating every API endpoint as a separate hand-written skill while still giving agents strict schemas and progressive disclosure.

## 8. Workflow-Backed Skills

Some skills need more than instructions and a curated tool set. A skill that
must orchestrate several tools, wait for human approval, retry failed steps,
run assertions, or preserve a durable audit trail should be backed by
`light-workflow`.

The boundary should stay clear:

| Layer | Responsibility |
| --- | --- |
| Skill | Discovery metadata, taxonomy, instructions, allowed tools, and agent guidance. |
| Workflow | Ordered execution, branching, retries, assertions, human tasks, durable state, and audit events. |
| Gateway | Runtime tool execution through `tools/list` and `tools/call`. |

Workflow backing should be optional. Simple skills can stay as instructions plus
tool mappings. Durable or regulated processes should link to workflow
definitions and let `light-workflow` own execution.

Recommended storage:

1. Keep `wf_definition_t.definition` as the canonical workflow YAML.
2. Keep `skill_t.content_markdown` as the LLM-facing skill instruction body.
3. Add `skill_workflow_t` to link skills to workflow definitions with a role
   such as `primary`, `validation`, `remediation`, or `test`.
4. Treat `skill_tool_t` as the allowed tool set for a workflow-backed skill.
   Validation should flag workflow tool-call steps that are not linked to the
   skill.

The Portal Skill Workspace should embed a generic Workflow Editor instead of
creating a skill-specific workflow runtime. The editor provides YAML editing,
step preview, reference lookup, validation, and test runs. Skill authoring
provides the surrounding context: skill metadata, taxonomy, allowed tools,
effective prompt preview, and workflow link configuration.

## 9. Next Steps
1. Complete phase 3 by adding category and tag assignment to existing skill create/update forms, backed by `entity_category_t` and `entity_tag_t` with `entity_type = 'skill'`.
2. Save skill taxonomy through a composite skill command so the skill row and selected taxonomy associations are emitted from the same user action.
3. Move the richer authoring workspace, effective prompt preview, `skill_tool_t.config` formalization, workflow-backed skills, and "create skill from LightAPI/tool" flows to phase 3.5.
4. Build the generic Workflow Editor for YAML editing, parsed step preview, catalog references, validation, and workflow test runs.
5. Complete phase 4 agent assignment by improving the `agent_skill_t` UI, adding an Agent Definition assignment context, and adding a batch assignment composite command that emits one `AgentSkillCreatedEvent` per selected skill.
6. Enforce phase 4 assignment validation in command handlers and UI preflight: assigned skills must be active and must have at least one active direct `skill_tool_t` link. Workflow-backed skills still rely on `skill_tool_t` as the allowed tool set.
7. Keep live gateway `tools/list` runtime executability checks as a diagnostics or governance concern, not as phase 4 persistence validation.
8. Complete phase 5 for the Rust agent with the `genai-query`
   `getEffectiveAgentCatalog` endpoint, claim checks against `host`, `sid`, and
   `env`, local catalog caching, keyword/routing search, gateway `tools/list`
   intersection, and controller-driven cache invalidation.
9. Complete phase 6 governance for the Rust agent only: normalize sensitivity
   tiers to `public`, `internal`, `confidential`, and `restricted`; filter
   blocked tools before catalog disclosure; compare the effective catalog with
   gateway `tools/list` through `/diagnostics/tools`; and keep execution
   through gateway `tools/call`.
10. Enforce destructive, approval-required, and sensitivity metadata at the
    gateway with debug/auditInfo fields when a call is blocked. Do not use
    workflow `audit_log_t` for catalog disclosure; use auditInfo/file logging
    until a generic governance audit table is introduced.
11. Keep current active row plus aggregate version as the approval/version
    boundary until workflow-owned approval state is implemented.
12. Add publishing from LightAPI endpoint descriptions into the skill registry.
13. Migrate existing file-based skills into structured catalog payloads, keeping instructions in Markdown and converting parameters to JSON Schema.
14. Implement Pattern B (Semantic Tool RAG) after indexed catalog fields and embeddings are ready for production search.
