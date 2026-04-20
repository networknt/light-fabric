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
*   **OpenAPI (YAML/JSON):** Used to map external REST APIs to skills.
*   **Executable Code (Python/JS) / URI:** Stores the actual execution logic or the endpoint reference.
*   **Markdown:** Retained *only* for the `instructions` or `prompt` fields, as LLMs excel at parsing markdown headers and lists for constraints and persona instructions.

### 3.1 Proposed Database Schema Structure
The centralized Controller will store skills in a structured table/collection. Below is a representation of the skill payload:

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
3.  **Discovery API:** The Controller's discovery service filters by these tags/categories to scoped skill sets for specific agent personas.

### 4.2 Progressive Disclosure Patterns
Agents will no longer load all skills at startup. Instead, the Controller will mediate access using one (or a combination) of the following patterns:

#### Pattern A: Meta-Tools (Dynamic Injection)
The agent is booted with only two "meta-tools" designed for discovery.
1.  `search_skills(query)`: Agent searches the DB. The Controller returns *lightweight summaries* (Name + Description only, no heavy schemas).
2.  `load_skill_schema(skill_name)`: Once the agent identifies the correct tool, it calls this. The Controller dynamically injects the heavy JSON schema into the context for the next turn.

#### Pattern B: Semantic Tool RAG (Zero-Shot Discovery)
For highly complex systems with thousands of skills:
1.  Tool descriptions are embedded into a Vector Database (e.g., `pgvector`).
2.  When the user prompts the system (e.g., "Reset my AWS password"), the Controller intercepts the prompt, performs a semantic search, and retrieves the Top-3 most relevant JSON Schemas.
3.  The agent boots with *only* those 3 tools in its context. 

#### Pattern C: Multi-Agent Orchestration (Supervisor / Worker)
Hierarchy is mapped to agent teams.
1.  A **Supervisor Agent** holds routing tools (e.g., `delegate_to_finance`, `delegate_to_devops`).
2.  When `delegate_to_devops` is triggered, the Controller spins up a **DevOps Worker Agent**, loading only the specific DevOps JSON schemas into its context.

---

## 5. Example Flow: Dynamic Loading in Action

**User:** *"I need to provision a new database for the marketing team."*

1.  **Turn 1: Discovery**
    *   *Agent Context:* Possesses only `search_skills(query)`.
    *   *Agent Action:* Calls `search_skills(query="provision database")`.
2.  **Turn 2: High-Level Awareness**
    *   *Controller Response:* Returns token-efficient summaries from the DB: 
        `[{"name": "aws_rds_provision", "description": "Creates AWS RDS DB"}, {"name": "mongo_atlas_create", "description": "Creates Mongo cluster"}]`
    *   *Agent Action:* Decides AWS is needed. Calls `load_skill_schema("aws_rds_provision")`.
3.  **Turn 3: Strict Execution**
    *   *Controller Response:* Injects the full JSON schema (requiring `instance_type`, `storage_gb`).
    *   *Agent Action:* Understands parameters and safely executes `aws_rds_provision` via the Controller's execution engine.

---

## 6. Operational Benefits & Security
By centralizing skills in a database, the platform gains enterprise-grade operational capabilities:
*   **Dynamic Updates:** API endpoints, instructions, and schemas can be updated in the database without restarting agents.
*   **Permission-Aware Discovery (RBAC):** By linking `tool_t` directly to `api_endpoint_t`, the Controller ensures that an agent only "discovers" tools that the current user/agent session is authorized to execute based on their roles.
*   **A/B Testing:** The Controller can route 50% of an agent's requests to `skill_v1` and 50% to `skill_v2` to measure prompt/tool efficacy.
*   **Audit Logging:** Every tool injection and execution is logged at the Controller level, establishing a single pane of glass for multi-agent compliance.
*   **Distilled Memory RAG:** Following the "Hindsight" pattern, raw conversation history (`agent_session_history_t`) is separated from RAG-optimized memory (`session_memory_t`). This prevents the "noisy context" problem while maintaining a perfect audit trail.

## 7. Next Steps
1. Provision the `agent_skills` table in the core database.
2. Build the API layer (Controller) to handle `search`, `retrieve`, and `execute` requests from agents.
3. Migrate existing Markdown-based skills into the structured DB payload (extracting prompts to the `instructions` field and converting parameters to JSON Schema).
4. Implement Pattern B (Semantic Tool RAG) as the default progressive disclosure mechanism.