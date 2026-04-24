# Database Design

The Light-Fabric utilizes a robust PostgreSQL schema to manage the entire lifecycle of agentic workflows, skills, and the biomimetic Hindsight memory system. The schema is organized into four logical layers:

## 1. Workflow Engine
These tables manage the definition and execution of long-running agentic workflows.

### `wf_definition_t`
Stores the Agentic Workflow DSL (YAML) that defines the high-level orchestration logic.

### `process_info_t` & `task_info_t`
Manage the runtime state of workflow instances (processes) and individual steps (tasks). They include `input_data`, `context_data`, and `error_info` to provide a resilient "scratchpad" for intermediate variables.

### `worklist_t` & `worklist_asst_t`
Manage task assignments and visibility for human-in-the-loop interactions.

---

## 2. Agentic Core (The "Brain & Skills")
These tables define the identity, expertise, and capabilities of individual agents.

### `agent_definition_t`
Defines the agent's persona, model provider (OpenAI, Anthropic, etc.), and runtime parameters like temperature and max tokens.

### `skill_t`
Stores the "Expertise" of an agent in Markdown format. Skills are hierarchical and versioned.

### `tool_t` & `tool_param_t`
The "Hands" of the agent. Defines executable functions, including REST endpoints, MCP server calls, or WASM scripts.

### `agent_skill_t` & `skill_tool_t`
Maps agents to skills and skills to tools, implementing the **Progressive Disclosure** pattern where agents only see the tools required for their current skill context.

---

## 3. Hindsight Memory System
A biomimetic memory architecture that transitions from flat logs to structured "atoms of thought."

### `agent_memory_bank_t`
Profiles for memory banks, defining the "Personality and Disposition" (e.g., skepticism, empathy) of the memory layer.

### `agent_memory_unit_t`
The individual "Atoms" of memory. Each unit contains content and a vector embedding (384-dim) for semantic retrieval.

### `agent_memory_entity_t` & `agent_memory_link_t`
A Knowledge Graph layer that resolves entities and causal/semantic relationships between memory units.

---

## 4. Session Management

### `agent_session_history_t`
The "Source of Truth" for active conversations, linking specific sessions to their respective Hindsight memory banks.

---

## DDL Specification

```sql
-- Workflow Definitions: Stores the Agentic Workflow JSON
CREATE TABLE wf_definition_t (
    host_id             UUID NOT NULL,
    wf_def_id           UUID NOT NULL,
    namespace           VARCHAR(126) NOT NULL,
    name                VARCHAR(126) NOT NULL,
    version             VARCHAR(20) NOT NULL,
    definition          TEXT NOT NULL, -- The Agentic Workflow DSL in YAML
    aggregate_version    BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT TRUE,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, wf_def_id),
    UNIQUE(host_id, namespace, name, version)
);

CREATE TABLE worklist_t (
  host_id              UUID NOT NULL,
  assignee_id          VARCHAR(126) NOT NULL,
  category_id          VARCHAR(126) DEFAULT '(all)' NOT NULL,
  status_code          VARCHAR(10) DEFAULT 'Active' NOT NULL,
  app_id               VARCHAR(512) DEFAULT 'global' NOT NULL,
  aggregate_version    BIGINT DEFAULT 1 NOT NULL,
  active               BOOLEAN NOT NULL DEFAULT TRUE,
  update_user          VARCHAR (255) DEFAULT SESSION_USER NOT NULL,
  update_ts            TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
  PRIMARY KEY(host_id, assignee_id, category_id)
);

CREATE TABLE worklist_column_t (
  host_id               UUID NOT NULL,
  assignee_id           VARCHAR(126) NOT NULL,
  category_id           VARCHAR(126) DEFAULT '(all)' NOT NULL,
  sequence_id           INTEGER NOT NULL,
  column_id             VARCHAR(126) NOT NULL,
  aggregate_version     BIGINT DEFAULT 1 NOT NULL,
  active                BOOLEAN DEFAULT TRUE,
  update_ts             TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
  update_user           VARCHAR(126) DEFAULT SESSION_USER,
  PRIMARY KEY(host_id, assignee_id, category_id, sequence_id),
  FOREIGN KEY(host_id, assignee_id, category_id) REFERENCES worklist_t(host_id, assignee_id, category_id) ON DELETE CASCADE
);

CREATE TABLE process_info_t (
  host_id                    UUID NOT NULL,
  process_id                 UUID NOT NULL, -- generated uuid
  wf_def_id                  UUID NOT NULL, -- workflow definition id
  wf_instance_id             VARCHAR(126)       NOT NULL, -- workflow intance id
  app_id                     VARCHAR(512)       NOT NULL, -- application id
  process_type               VARCHAR(126)      NOT NULL,
  status_code                CHAR(1)            NOT NULL, -- process status code 'A', 'C'
  started_ts                 TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
  ex_trigger_ts              TIMESTAMP WITH TIME ZONE          NOT NULL,
  custom_status_code         VARCHAR(126),
  completed_ts               TIMESTAMP WITH TIME ZONE,
  result_code                VARCHAR(126),
  source_id                  VARCHAR(126),
  branch_code                VARCHAR(126),
  rr_code                    VARCHAR(126),
  party_id                   VARCHAR(126),
  party_name                 VARCHAR(126),
  counter_party_id           VARCHAR(126),
  counter_party_name         VARCHAR(126),
  txn_id                     VARCHAR(126),
  txn_name                   VARCHAR(126),
  product_id                 VARCHAR(126),
  product_name               VARCHAR(126),
  product_type               VARCHAR(126),
  group_name                 VARCHAR(126),
  subgroup_name              VARCHAR(126),
  event_start_ts             TIMESTAMP WITH TIME ZONE,
  event_end_ts               TIMESTAMP WITH TIME ZONE,
  event_other_ts             TIMESTAMP WITH TIME ZONE,
  event_other                VARCHAR(126),
  risk                       NUMERIC,
  risk_scale                 INTEGER,
  price                      NUMERIC,
  price_scale                INTEGER, -- Scale (number of digits to the right of the decimal) of the risk column. NULL implies zero
  product_qy                 NUMERIC,
  currency_code              CHAR(3),
  ex_ref_id                  VARCHAR(126),
  ex_ref_code                VARCHAR(126),
  product_qy_scale           INTEGER,
  parent_process_id          VARCHAR(22),
  deadline_ts                TIMESTAMP WITH TIME ZONE,
  parent_group_id            NUMERIC,
  process_subtype_code       VARCHAR(126),
  owning_group_name          VARCHAR(126), -- Name of the group that owns the process
  input_data                 JSONB,        -- The initial data that triggered the workflow
  context_data               JSONB,        -- The runtime "scratchpad" for intermediate variables
  error_info                 TEXT,         -- Detailed error or stack trace if the process fails
  aggregate_version   BIGINT DEFAULT 1 NOT NULL,
  active              BOOLEAN DEFAULT TRUE,
  update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
  update_user         VARCHAR(126) DEFAULT SESSION_USER,
  PRIMARY KEY(host_id, process_id),
  FOREIGN KEY(host_id, wf_def_id) REFERENCES wf_definition_t(host_id, wf_def_id) ON DELETE CASCADE
);

CREATE TABLE task_info_t
(
    host_id             UUID NOT NULL,
    task_id             UUID NOT NULL,
    task_type           VARCHAR(126) NOT NULL,
    process_id          UUID NOT NULL,
    wf_instance_id      VARCHAR(126) NOT NULL,
    wf_task_id          VARCHAR(126) NOT NULL,
    status_code         CHAR(1)       NOT NULL, -- U, A, C
    started_ts          TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    locked              CHAR(1)       NOT NULL,
    priority            INTEGER        NOT NULL,
    completed_ts        TIMESTAMP WITH TIME ZONE      NULL,
    completed_user      VARCHAR(126)     NULL,
    result_code         VARCHAR(126)     NULL,
    locking_user        VARCHAR(126)     NULL,
    locking_role        VARCHAR(126)     NULL,
    deadline_ts         TIMESTAMP WITH TIME ZONE      NULL,
    lock_group          VARCHAR(126)     NULL,
    task_input          JSONB,           -- Specific data passed to the task
    task_output         JSONB,           -- Result returned by the task action
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT TRUE,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, task_id),
    FOREIGN KEY (host_id, process_id) REFERENCES process_info_t(host_id, process_id) ON DELETE CASCADE
);

CREATE TABLE task_asst_t
(
    host_id             UUID NOT NULL,
    task_asst_id         UUID NOT NULL,
    task_id              UUID NOT NULL,
    assigned_ts          TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    assignee_id          VARCHAR(126) NOT NULL,
    reason_code          VARCHAR(126) NOT NULL,
    unassigned_ts        TIMESTAMP WITH TIME ZONE      NULL,
    unassigned_reason    VARCHAR(126)     NULL,
    category_code        VARCHAR(126)     NULL,
    aggregate_version    BIGINT DEFAULT 1 NOT NULL,
    active               BOOLEAN DEFAULT TRUE,
    update_ts            TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user          VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, task_asst_id),
    FOREIGN KEY(host_id, task_id) REFERENCES task_info_t(host_id, task_id) ON DELETE CASCADE
);

CREATE TABLE audit_log_t
(
    host_id             UUID NOT NULL,
    audit_log_id        UUID NOT NULL,
    source_type_id      VARCHAR(126)      NULL,
    correlation_id      VARCHAR(126)      NULL,
    user_id             VARCHAR(126)     NULL,
    event_ts            TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    success             CHAR(1)           NULL,
    message0            VARCHAR(126)     NULL,
    message1            VARCHAR(126)     NULL,
    message2            VARCHAR(126)     NULL,
    message3            VARCHAR(126)     NULL,
    message             VARCHAR(500)     NULL,
    user_comment        VARCHAR(500)     NULL,
    PRIMARY KEY(host_id, audit_log_id)
);

CREATE INDEX audit_log_idx1 ON audit_log_t (source_type_id, correlation_id, event_ts, user_id);

-- Agent Definitions: Stores the "Brain" configuration
CREATE TABLE agent_definition_t (
    host_id             UUID NOT NULL,
    agent_def_id        UUID NOT NULL,
    agent_name          VARCHAR(126) NOT NULL,
    model_provider      VARCHAR(64) NOT NULL,  -- 'openai', 'anthropic', etc.
    model_name          VARCHAR(126) NOT NULL, -- 'gpt-4o', 'claude-3-5-sonnet'
    api_key_ref         VARCHAR(126),          -- Reference to Secret Manager key
    temperature         NUMERIC(3,2) DEFAULT 0.7,
    max_tokens          INTEGER,               -- max number of tokens can be used
    aggregate_version    BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT TRUE,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, agent_def_id),
    UNIQUE(host_id, agent_name)
);


-- Skills: Stores Instructions and Domain Knowledge (The "Expertise")
-- Note: Use entity_tag_t and entity_category_t with entity_type = 'skill' 
-- for flat tagging and hierarchical folder structure of skills.
CREATE TABLE skill_t (
    host_id             UUID NOT NULL,
    skill_id            UUID NOT NULL,
    parent_skill_id     UUID,                  -- Self-reference for Hierarchy
    name                VARCHAR(126) NOT NULL,
    description         VARCHAR(500),          -- High-level description for the initial LLM prompt
    content_markdown    TEXT NOT NULL,         -- The actual instructions/prompts

    description_embedding VECTOR(384),          -- For semantic lookup/discovery
    version             VARCHAR(20) DEFAULT '1.0.0',
    aggregate_version    BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, skill_id),
    FOREIGN KEY(host_id, parent_skill_id) REFERENCES skill_t(host_id, skill_id)
);

CREATE INDEX idx_skill_active ON skill_t(active);
CREATE INDEX idx_skill_name ON skill_t(name);

-- Tools: Stores Executable Functions (The "Hands")
CREATE TABLE tool_t (
    host_id             UUID NOT NULL,
    tool_id             UUID NOT NULL,
    name                VARCHAR(126) NOT NULL,
    description         TEXT NOT NULL,         -- Instructions for LLM on when/how to use this tool

    -- Implementation specifics
    implementation_type VARCHAR(50),           -- 'java', 'mcp_server', 'rest', 'python', 'javascript'
    implementation_class VARCHAR(500),         -- FQCN if 'java'
    mcp_server_name      VARCHAR(126),         -- MCP server name if 'mcp_server'
    api_endpoint        VARCHAR(1024),         -- URL if 'rest'
    api_method          VARCHAR(10),           -- HTTP Method if 'rest'
    endpoint_id         UUID,                  -- Reference to fine-grained auth endpoint
    script_content      TEXT,                  -- Source code if 'python'/'javascript'
    response_schema     JSONB,                 -- Strict output schema for tool results

    description_embedding VECTOR(384),          -- For semantic lookup/discovery
    version             VARCHAR(20) DEFAULT '1.0.0',
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, tool_id),
    FOREIGN KEY(host_id, endpoint_id) REFERENCES api_endpoint_t(host_id, endpoint_id) ON DELETE CASCADE
);

CREATE INDEX idx_tool_host_endpoint ON tool_t(host_id, endpoint_id);
CREATE INDEX idx_tool_active ON tool_t(active);
CREATE INDEX idx_tool_name ON tool_t(name);

-- Tool Parameters: Defines the arguments for each tool
CREATE TABLE tool_param_t (
    host_id             UUID NOT NULL,
    param_id            UUID NOT NULL,
    tool_id             UUID NOT NULL,
    name                VARCHAR(255) NOT NULL,
    param_type          VARCHAR(50) NOT NULL,      -- 'string', 'number', 'boolean', 'object', 'array'
    required            BOOLEAN DEFAULT true,
    default_value       JSONB,
    description         TEXT,                      -- Helps LLM understand what value to extract
    validation_schema   JSONB,                     -- JSON Schema for complex validation
    order_index         INTEGER DEFAULT 0,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, param_id),
    FOREIGN KEY(host_id, tool_id) REFERENCES tool_t(host_id, tool_id) ON DELETE CASCADE
);

-- Skill Dependencies: Manages hierarchies where one skill requires another
CREATE TABLE skill_dependency_t (
    host_id             UUID NOT NULL,
    skill_id            UUID NOT NULL,
    depends_on_skill_id UUID NOT NULL,
    required            BOOLEAN DEFAULT true,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY (host_id, skill_id, depends_on_skill_id),
    FOREIGN KEY(host_id, skill_id) REFERENCES skill_t(host_id, skill_id),
    FOREIGN KEY(host_id, depends_on_skill_id) REFERENCES skill_t(host_id, skill_id)
);

-- Agent-Skill Mapping: Links Agents to their Skills
CREATE TABLE agent_skill_t (
    host_id             UUID NOT NULL,
    agent_def_id        UUID NOT NULL,
    skill_id            UUID NOT NULL,

    config              JSONB DEFAULT '{}',
    priority            INTEGER DEFAULT 0,
    sequence_id         INTEGER DEFAULT 0,     -- Order in which skills are concatenated

    aggregate_version    BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, agent_def_id, skill_id),
    FOREIGN KEY(host_id, agent_def_id) REFERENCES agent_definition_t(host_id, agent_def_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id, skill_id) REFERENCES skill_t(host_id, skill_id) ON DELETE CASCADE
);
CREATE INDEX idx_agent_skill_agent ON agent_skill_t(agent_def_id);

-- Skill-Tool Mapping: Implements Progressive Disclosure
CREATE TABLE skill_tool_t (
    host_id             UUID NOT NULL,
    skill_id            UUID NOT NULL,
    tool_id             UUID NOT NULL,

    config              JSONB DEFAULT '{}',
    access_level        VARCHAR(20) DEFAULT 'read', -- e.g., 'read', 'write', 'execute'

    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, skill_id, tool_id),
    FOREIGN KEY(host_id, skill_id) REFERENCES skill_t(host_id, skill_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id, tool_id) REFERENCES tool_t(host_id, tool_id) ON DELETE CASCADE
);
CREATE INDEX idx_skill_tool_skill ON skill_tool_t(skill_id);

-- -- Hindsight Advanced Memory System
-- Transitioned from flat logs to biomimetic memory banks (World, Experiences, Mental Models)

-- Memory bank profiles (Personality & Disposition)
CREATE TABLE agent_memory_bank_t (
    host_id             UUID NOT NULL,
    bank_id             UUID NOT NULL,
    agent_def_id        UUID,                  -- NULL if bank is shared across agents
    user_id             UUID,                  -- NULL if bank is global for the host/agent
    bank_name           VARCHAR(126) NOT NULL,
    disposition         JSONB NOT NULL DEFAULT '{"skepticism": 3, "literalism": 3, "empathy": 3}'::jsonb,
    background          TEXT,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, bank_id),
    FOREIGN KEY(host_id) REFERENCES host_t(host_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id, agent_def_id) REFERENCES agent_definition_t(host_id, agent_def_id) ON DELETE CASCADE,
    FOREIGN KEY(user_id) REFERENCES user_t(user_id) ON DELETE CASCADE
);

-- Source documents for memory units
CREATE TABLE agent_memory_doc_t (
    host_id             UUID NOT NULL,
    doc_id              UUID NOT NULL,
    bank_id             UUID NOT NULL,
    original_text       TEXT,
    content_hash        TEXT,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY (host_id, bank_id, doc_id),
    FOREIGN KEY (host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE
);

-- Individual sentence-level memories (The "Atoms" of thought)
CREATE TABLE agent_memory_unit_t (
    host_id             UUID NOT NULL,
    unit_id             UUID NOT NULL,
    bank_id             UUID NOT NULL,
    doc_id              UUID,
    content             TEXT NOT NULL,
    embedding           vector(384),
    context             TEXT,
    event_date          TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT now(),
    occurred_start      TIMESTAMP WITH TIME ZONE,
    occurred_end        TIMESTAMP WITH TIME ZONE,
    mentioned_at        TIMESTAMP WITH TIME ZONE,
    fact_type           VARCHAR(32) NOT NULL DEFAULT 'world' CHECK (fact_type IN ('world', 'experience', 'opinion', 'observation', 'mental_model')),
    metadata            JSONB DEFAULT '{}'::jsonb,
    proof_count         INT DEFAULT 1,
    source_memory_ids   UUID[] DEFAULT ARRAY[]::UUID[],
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, bank_id, unit_id),
    FOREIGN KEY(host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE,
    FOREIGN KEY(host_id, bank_id, doc_id) REFERENCES agent_memory_doc_t(host_id, bank_id, doc_id) ON DELETE CASCADE
);

CREATE INDEX idx_mem_unit_bank ON agent_memory_unit_t(bank_id);
CREATE INDEX idx_mem_unit_embedding ON agent_memory_unit_t USING hnsw (embedding vector_cosine_ops);

-- Resolved entities (Knowledge Graph Nodes)
CREATE TABLE agent_memory_entity_t (
    host_id             UUID NOT NULL,
    entity_id           UUID NOT NULL,
    bank_id             UUID NOT NULL,
    user_id             UUID,                  -- Link to user_t if this entity is a platform user
    canonical_name      TEXT NOT NULL,
    mention_count       INT DEFAULT 1,
    metadata            JSONB DEFAULT '{}'::jsonb,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY (host_id, bank_id, entity_id),
    FOREIGN KEY (host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES user_t(user_id) ON DELETE CASCADE
);

-- Association between memory units and entities
CREATE TABLE agent_memory_unit_entity_t (
    host_id             UUID NOT NULL,
    bank_id             UUID NOT NULL,
    unit_id             UUID NOT NULL,
    entity_id           UUID NOT NULL,
    PRIMARY KEY (host_id, bank_id, unit_id, entity_id),
    FOREIGN KEY (host_id, bank_id, unit_id) REFERENCES agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE,
    FOREIGN KEY (host_id, bank_id, entity_id) REFERENCES agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE
);

-- Cache of entity co-occurrences (Concept Relationship Graph)
CREATE TABLE agent_memory_entity_cooccur_t (
    host_id             UUID NOT NULL,
    bank_id             UUID NOT NULL,
    entity_id_1         UUID NOT NULL,
    entity_id_2         UUID NOT NULL,
    cooccur_count       INT DEFAULT 1,
    last_cooccurred     TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY (host_id, bank_id, entity_id_1, entity_id_2),
    CONSTRAINT entity_cooccur_order_check CHECK (entity_id_1 < entity_id_2),
    FOREIGN KEY (host_id, bank_id, entity_id_1) REFERENCES agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE,
    FOREIGN KEY (host_id, bank_id, entity_id_2) REFERENCES agent_memory_entity_t(host_id, bank_id, entity_id) ON DELETE CASCADE
);

CREATE INDEX idx_mem_cooccur_e1 ON agent_memory_entity_cooccur_t(host_id, entity_id_1);
CREATE INDEX idx_mem_cooccur_e2 ON agent_memory_entity_cooccur_t(host_id, entity_id_2);

-- Links between memory units (Semantic & Causal relationships)
CREATE TABLE agent_memory_link_t (
    host_id             UUID NOT NULL,
    bank_id             UUID NOT NULL,
    from_unit_id        UUID NOT NULL,
    to_unit_id          UUID NOT NULL,
    link_type           VARCHAR(32) NOT NULL,
    weight              FLOAT NOT NULL DEFAULT 1.0,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY (host_id, bank_id, from_unit_id, to_unit_id, link_type),
    CONSTRAINT memory_links_type_check CHECK (link_type IN ('temporal', 'semantic', 'entity', 'causes', 'caused_by', 'enables', 'prevents')),
    FOREIGN KEY (host_id, bank_id, from_unit_id) REFERENCES agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE,
    FOREIGN KEY (host_id, bank_id, to_unit_id) REFERENCES agent_memory_unit_t(host_id, bank_id, unit_id) ON DELETE CASCADE
);

-- Directives (Hard rules that override probabilistic learning)
CREATE TABLE agent_memory_directive_t (
    host_id             UUID NOT NULL,
    directive_id        UUID NOT NULL,
    bank_id             UUID NOT NULL,
    name                VARCHAR(256) NOT NULL,
    content             TEXT NOT NULL,
    priority            INT NOT NULL DEFAULT 0,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, bank_id, directive_id),
    FOREIGN KEY(host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE
);

-- Reflections (Synthesized knowledge and high-level observations)
CREATE TABLE agent_memory_reflection_t (
    host_id             UUID NOT NULL,
    reflection_id       UUID NOT NULL,
    bank_id             UUID NOT NULL,
    content             TEXT NOT NULL,
    embedding           vector(384),
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, bank_id, reflection_id),
    FOREIGN KEY(host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE
);

CREATE INDEX idx_mem_reflection_embedding ON agent_memory_reflection_t USING hnsw (embedding vector_cosine_ops);

-- Raw Session History (The source of Truth for active conversations)
CREATE TABLE agent_session_history_t (
    host_id             UUID NOT NULL,
    session_id          UUID NOT NULL,
    bank_id             UUID NOT NULL,         -- Links the session to a Hindsight bank
    messages            JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata            JSONB DEFAULT '{}'::jsonb,
    aggregate_version   BIGINT DEFAULT 1 NOT NULL,
    active              BOOLEAN DEFAULT true,
    update_ts           TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    update_user         VARCHAR(126) DEFAULT SESSION_USER,
    PRIMARY KEY(host_id, bank_id, session_id),
    FOREIGN KEY(host_id, bank_id) REFERENCES agent_memory_bank_t(host_id, bank_id) ON DELETE CASCADE
);

CREATE INDEX idx_session_bank ON agent_session_history_t(host_id, bank_id);


```
