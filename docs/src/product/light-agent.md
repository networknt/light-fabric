# Light-Agent

`light-agent` is the interactive agent service in Light Fabric.

It provides a WebSocket chat interface, integrates with model providers,
invokes MCP tools through `mcp-client`, and stores conversation memory through
`hindsight-client`. The current executable implements the enterprise
API/MCP-oriented service path. Coding and personal-assistant support extend the
same durable agent domain through additional runtime profiles rather than
forking separate agent engines.

## Execution Model

`light-agent` is a long-lived interactive session service. A logical agent does
not automatically receive its own container or VM.

Remote model calls and gateway-only API/MCP tools can remain in the service.
Turns that need a local CLI model, shell, browser, filesystem, repository,
private local MCP server, or other effectful tenant execution use a
runner-managed backend selected from server-owned policy. High-value publish,
signing, deployment, branch, and pull-request operations use fixed structured
actions.

Tool availability is placement-specific: gateway catalog entries intersect
live gateway `tools/list`, while runner-local shell/filesystem/browser/local-MCP
entries intersect the execution profile, lease allowlist, approved runtime
manifest, and live local availability. The independently authorized sets can
be combined for the model, but a tool remains bound to one server-owned
placement and dispatcher.

Human approval ends the current action lease and credentials. A task sandbox is
cleaned; an eligible non-secret coding-session workspace may instead use a
separate bounded pause/checkpoint hold. The hold is not executable authority
and cannot extend the session maximum lifetime.

The target profiles are:

- enterprise business agents: long-lived light-agent reasoning plus typed
  light-gateway API/MCP tools;
- coding agents: a bounded `light-agent-worker` inside a runner-managed
  workspace sandbox, using a native or external agent runtime adapter;
- personal assistants: light-agent reasoning plus a separately deployed
  `light-agent-channel` for messaging and proactive triggers, with an optional
  personal edge runner for local-device effects.

Codex, Pi, Claude Code, Gemini CLI, Kilo, Hermes, OpenClaw, and similar
harnesses are integration candidates behind an agent-runtime adapter. They are
not launched directly by the shared light-agent service or by light-workflow.
Centralized skills are materialized for the selected profile, but never grant
execution authority by themselves.

See [Light-Agent Execution](../design/light-agent-execution.md) for session and
turn durability, tool authorization, sandbox placement, deployment profiles,
runtime adapters, channel ingress, workflow handoffs, and the origin-neutral
runner contract shared with workflow execution. See
[Centralized Skills](../design/centralized-agent-skills.md) for profile-specific
skill materialization.

## Key Dependencies

- `light-runtime`
- `light-axum`
- `model-provider`
- `mcp-client`
- `hindsight-client`
- `portal-registry`

## Runtime

The app follows the standard runtime pattern:

- load config from `config/`
- implement an Axum app
- start through `LightRuntimeBuilder`
- optionally register through portal registry
