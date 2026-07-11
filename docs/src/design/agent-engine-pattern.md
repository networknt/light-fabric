# Agent Engine Pattern

The **Agent Engine Pattern** is the architectural standard for building industrial-grade, metadata-driven AI platforms within the Light-Fabric ecosystem.

In this model, the **Rust Runtime** acts as a high-performance **Orchestrator**, while the **Application Logic** resides in externalized metadata (JSON/YAML) and the **Hindsight Memory** database.

## 1. Why the Metadata-Driven Approach?

- **Separation of Concerns**: Complex platform logic (security, retries, database connectivity, LLM integration) is implemented once in Rust. Business logic—defining agent personas, goals, and steps—is "programmed" via JSON or Database records.
- **Hot-Reloading**: Using the `arc-swap` crate and YAML-based rule engines, agent personas, model parameters, and tool access can be updated in real-time without a server restart.
- **Elastic Scalability**: Deploy one shared agent engine and specialize it from
  registry metadata. The public `light-agent` service owns sessions and
  reasoning, `light-agent-worker` hosts sandboxed coding/runtime adapters, and
  the optional `light-agent-channel` owns messaging connections. These are thin
  trust-boundary executables over shared domain crates, not separate persona
  engines.
- **High Performance**: Rust's asynchronous `tokio` runtime allows a single engine instance to manage thousands of concurrent agentic sessions with minimal memory overhead.

## 2. The Core Architecture: Engine vs. Content

To function as a generic interpreter, the Light-Fabric Engine relies on four primary components:

### A. The Tool & Skill Registry (The "Hands")
The engine maps string identifiers in the workflow JSON (e.g., `"call": "get_customer_data"`) to governed API/MCP capabilities, fixed actions, or immutable sandbox packages.
- **Implementation**: Uses a `ToolRegistry` with trait objects (`Box<dyn Tool>`) or dynamic dispatch to MCP (Model Context Protocol) servers.
- **Logic**: When the LLM requests a tool call, the engine verifies permissions via **Fine-Grained Authorization**, executes the tool, and feeds the result back into the context.

The registry is not an authorization or execution boundary. Mutable script
source is never trusted because it appears in metadata; executable packages
must be content-addressed, reviewed, and run through an approved
`ExecutionBackend`.

### B. Hindsight State Manager (The "Memory")
Unlike simple session storage, the state manager persists every step of the agentic interaction into biomimetic memory banks.
- **Implementation**: Every "turn" in the conversation is saved as a `unit_t` in the Hindsight database.
- **Benefit**: Provides fault tolerance (resuming from a crashed step) and "Recall" capabilities, allowing agents to remember past interactions across different sessions.

### C. Prompt Templating (The "Mind")
System prompts and instructions are stored as templates rather than hardcoded strings.
- **Implementation**: Uses the `tera` or `rinja` engines for high-performance string interpolation.
- **Example**: `"You are a {{agent_role}}. Your current objective is to {{agent_goal}}."`
- **Rust Logic**: The engine merges runtime context (user input, memory recall, tool results) into the template before calling the LLM.

### D. Policy Engine (The "Shield")
Before any tool execution or data retrieval, the engine consults the **Light-Rule** middleware.
- **Logic**: Ensures the agent has the authority to access specific data or execute specific functions, preventing "prompt injection" from leading to unauthorized actions.

## 3. Conceptual Implementation in Rust

The `AgentEngine` in Light-Fabric follows a non-blocking, async loop:

```rust
pub struct AgentEngine {
    registry: Arc<ToolRegistry>,
    memory: Arc<HindsightClient>,
    rules: Arc<RuleEngine>,
}

impl AgentEngine {
    pub async fn execute_step(&self, session_id: Uuid, task: Task) -> anyhow::Result<()> {
        // 1. Fetch current context from Hindsight Memory
        let mut context = self.memory.get_context(session_id).await?;

        // 2. Resolve Task Type (Agentic vs. Tool Call)
        match task {
            Task::LlmCall { agent_id, prompt_template } => {
                // Render prompt with Tera
                let prompt = self.render_prompt(prompt_template, &context)?;

                // Call LLM Provider
                let response = self.llm_provider.chat(prompt, &context).await?;

                // Retain turn in Hindsight
                self.memory.retain_turn(session_id, response).await?;
            },
            Task::ToolCall { tool_name, params } => {
                // 3. Enforce Fine-Grained Authorization
                if self.rules.authorize(session_id, &tool_name).await? {
                    let result = self.registry.call(&tool_name, params).await?;
                    context.add_result(tool_name, result);
                }
            }
        }

        // 4. Update Session State
        self.memory.checkpoint(session_id, context).await
    }
}
```

## 4. Operational Challenges & Solutions

1.  **Tool Versioning**: As the platform evolves, tools may change. Light-Fabric handles this by versioning tool definitions in the Registry, ensuring old workflows remain compatible with the tools they were designed for.
2.  **Safe Execution**: A logical agent does not automatically own a sandbox. Remote model and gateway-only work can remain in the long-lived service; shell, filesystem, browser, local MCP, CLI-model, or untrusted execution uses an approved `ExecutionBackend` such as a microVM, rootless container, Kubernetes Job, dedicated VM, or fixed external action. The effective policy must match the backend's proven boundary.
3.  **Observability**: Because the engine is generic, tracing is built into `light-runtime`. Traces record session, turn, model-call, tool-action, policy, lease, and result metadata without treating private hidden reasoning as an observable platform contract.

## The Recommendation

Light-Fabric adopts this **"Engine-first"** philosophy to keep one durable
agent model across enterprise, coding, and personal-assistant profiles. Agent
definitions and skills are data; shared Rust crates implement sessions,
policy, memory, runtime protocols, and audit; thin service, sandbox-worker, and
channel binaries enforce their distinct lifecycles and trust boundaries.

See [Light-Agent Execution](light-agent-execution.md) for the concrete
service, session, turn, tool, runner, and sandbox boundaries.
