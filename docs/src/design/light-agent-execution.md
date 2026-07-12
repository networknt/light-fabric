# Light-Agent Execution

## Status

Proposed.

This design defines how interactive Light agents are hosted, how agent turns
and tool actions are authorized and recovered, and when execution must move
from a long-lived agent service into a runner-managed sandbox.

It complements:

- [Light-Workflow Runner](light-workflow-runner.md)
- [Execution Backends And Sandbox Execution](../product/light-workflow/sandbox-execution.md)
- [Agent Engine Pattern](agent-engine-pattern.md)
- [Hindsight Memory](hindsight-memory.md)

## Decision

A logical agent is not an isolation unit.

Do not create a container, VM, or sandbox for every agent definition by
default. Use a hybrid model:

- run interactive API-based agents in a long-lived light-agent service;
- group service instances by tenant, trust, model-provider, network, and data
  boundary;
- execute local CLI providers, code, shell, browser, filesystem, private local
  MCP, and other effectful work through the shared controller/runner execution
  substrate;
- select the sandbox backend from server-owned policy and runner capabilities;
- use a dedicated VM or external fixed service only when the workload,
  credential, regulatory, or host-exposure requirement justifies it.

The same agent session may use more than one execution boundary. A remote model
call can remain in the service, an HTTP or MCP API tool can run through
light-gateway, a code-repair action can run in a Cube or Docker sandbox, and a
publish action can run in a separate fixed service.

Support three product profiles through the same agent control plane:

- enterprise business agents use remote model providers and typed API/MCP
  tools through light-gateway;
- coding agents run a workspace-aware model/tool loop in a runner-managed
  sandbox;
- personal assistants use the same session, memory, policy, and skill model,
  but receive messages and proactive triggers through a separately deployed
  channel gateway and use an edge runner for local-device effects.

These are runtime profiles, not forks of the agent engine. Share the durable
agent domain model, policy evaluator, skill resolver, runtime protocol, and
audit vocabulary. Add a separate executable only where the trust boundary or
process lifecycle is materially different.

## Problem

Agent definitions, chat sessions, model calls, tool calls, and local execution
have different lifecycles and trust boundaries. Treating all of them as one
process creates two bad extremes:

1. one shared process receives every tenant credential, workspace, tool, and
   side effect; or
2. every logical agent permanently owns a container or VM even when it only
   makes a bounded remote model call.

The first is unsafe. The second is expensive, slow to scale, and ties metadata
to infrastructure unnecessarily.

Agent execution also differs from a workflow task:

- a session can contain many turns;
- a turn can contain several model calls and tool actions;
- the client expects interactive streaming and cancellation;
- multiple turns may share conversation memory;
- a coding session may optionally reuse a workspace;
- an agent can ask for human approval without keeping compute alive.

The execution substrate can be shared with workflows, but session and turn
orchestration remain owned by light-agent.

## Current Runtime Boundary

The current apps/light-agent executable is a long-lived Axum service.

At startup it creates one process-wide AgentState containing:

- one model provider and model;
- one MCP gateway client;
- one portal query client and portal credential;
- one PostgreSQL pool and memory store;
- one host identity;
- one optional agent definition ID;
- one catalog cache.

The service exposes a WebSocket chat route. Each connection:

1. accepts or creates a session ID;
2. uses the session UUID as its memory-bank ID;
3. loads conversation history;
4. accepts user messages sequentially on that socket;
5. recalls memory;
6. selects portal catalog tools and, because every currently executable entry
   is gateway-placed, intersects them with gateway tools/list;
7. runs up to ten model/tool iterations;
8. calls selected tools through light-gateway;
9. persists the final conversation history and experience.

Docker Compose and Kubernetes deploy light-agent as a persistent service. The
current account, advisor, and technical-support scripts use distinct service
identities, which makes one deployment per configured agent profile the
practical short-term model.

This implementation is a useful service foundation, but it is not yet a
durable or strongly isolated agent execution engine.

## Current Gaps

The first implementation work must close these gaps before broad multi-user or
effectful use:

1. A caller-supplied sessionId is not bound inside light-agent to an
   authenticated user and agent definition before memory is loaded.
2. The existing memory schema can store user_id and agent_def_id, but current
   session-bank creation does not populate those ownership fields.
3. Catalog selection limits the tool specifications shown to the model, but a
   returned tool name is not revalidated against the accepted set immediately
   before tools/call.
4. Tool arguments fall back to an empty object on malformed JSON instead of
   failing closed and being checked against the selected input schema.
5. There is no durable agent-turn or tool-attempt record. A crash after an
   effectful tool call and before history persistence can leave an unknown
   outcome that a reconnect may repeat.
6. Concurrent connections using the same session can race history updates.
7. Turn-level deadlines, token/cost budgets, tool-call budgets, cancellation,
   output limits, and concurrency quotas are incomplete.
8. Tool results are inserted into model context without a strict byte/token
   limit or an explicit untrusted-content boundary.
9. CLI providers spawn local child processes. A child inherits the service
   environment unless explicitly scrubbed and can therefore see process-wide
   credentials.
10. Claude Code agent mode currently requests its permission-bypass mode. It
    must never run inside a shared credential-bearing agent service.
11. Local helper scripts contain default bearer-token literals. Those values
    must be removed and rotated regardless of whether they were intended only
    for development.
12. Portal-command is the production memory-write default. Direct PostgreSQL
    writes are retained only as an explicitly enabled local/development
    compatibility mode.
13. The current MCP client path forwards the caller Authorization header to
    light-gateway. It does not yet exchange it for a token narrowed to the
    agent, turn/action, tool, data boundary, and policy digest.

## Goals

- Preserve low-latency interactive chat and streaming.
- Keep logical agent definitions independent from deployment units.
- Bind every session and turn to authenticated tenant, host, user, agent, and
  policy identities.
- Serialize concurrent same-session prompts through a bounded durable queue.
- Provide durable, idempotent turn and tool-action state.
- Reuse runner registration, scheduling, leases, fencing, watchdog, artifact,
  credential, and backend contracts.
- Keep workflow tasks and agent turns under their respective orchestrators.
- Route remote API tools through light-gateway.
- Downscope caller authority to the exact agent turn/action and data boundary
  before gateway dispatch.
- Route local or effectful execution through an approved ExecutionBackend.
- Support task-scoped and bounded session-scoped sandboxes.
- Support enterprise, coding, and personal-assistant profiles without forking
  the agent domain model.
- Treat Codex, Pi, Claude Code, Gemini CLI, Kilo, and similar products as agent
  runtime adapters rather than ordinary model providers.
- Materialize one centrally governed skill into profile-specific prompt,
  schema, package, and sandbox inputs.
- Normalize messaging channels and proactive triggers into authenticated,
  idempotent agent turns.
- Allow typed agent-to-workflow and workflow-to-agent handoffs without moving
  interactive turn ownership into light-workflow.
- Fail closed when a deployment cannot satisfy the required boundary.
- Release action leases, model channels, and action credentials while waiting
  for human approval. Clean task sandboxes; retain a non-secret session
  workspace only through a distinct bounded hold/checkpoint policy.

## Non-Goals

- Do not convert every chat message into a workflow instance.
- Do not give every agent definition a permanent container or VM.
- Do not make controller-rs the owner of conversation or workflow state.
- Do not let the model choose its isolation boundary or credentials.
- Do not let local runner configuration weaken server-owned policy.
- Do not treat a Docker container, Kubernetes pod, or Toolbx as a universal
  security boundary.
- Do not expose publish, signing, deployment, or unrestricted shell credentials
  to a general agent loop.
- Do not require a sandbox for a bounded remote model call with no local
  effects.
- Do not rely on the UI disabling the composer to serialize session mutations.
- Do not use a workflow instance as the inner loop for every chat message,
  coding command, or personal-assistant action.
- Do not let light-workflow or a channel gateway directly spawn an external
  agent CLI.
- Do not treat a skill package, repository instruction, plugin, or generated
  skill as authorization to gain tools, credentials, network, or filesystem
  access.
- Do not place messaging-channel credentials, model-provider credentials,
  tenant API credentials, and unrestricted local-device access in one shared
  process.

## Concepts

### Agent Definition

Versioned metadata describing instructions, skills, model policy, tool policy,
memory policy, data boundary, and default execution profile.

An agent definition is content. It does not own a process.

### Agent Product Profile

A server-owned profile selecting the turn lifecycle, ingress surfaces, runtime
placement, default tools, memory policy, sandbox requirements, and deployment
boundary for an agent definition.

The initial values are enterprise, coding, and personal-assistant. A profile
narrows the effective policy; it does not grant authority by itself.

### Agent Runtime Adapter

A versioned adapter that runs one model/tool loop and emits normalized runtime
events. Native light-agent reasoning, Pi RPC/SDK, Codex, Claude Code, Gemini
CLI, and other external harnesses implement this boundary.

An agent runtime is not a model provider. A model provider performs inference;
an agent runtime may own a session, tools, local state, approvals, and repeated
model calls.

### Agent Runtime Host

The small `light-agent-worker` executable launched inside a runner-managed
sandbox. It verifies the leased runtime specification, materializes approved
skills and context, starts exactly one runtime adapter, streams normalized
events, and exits or checkpoints at the lease boundary.

It does not authenticate end users, own conversation history, choose policy,
or accept arbitrary executable paths from a prompt.

### Channel Gateway

The optional `light-agent-channel` executable that owns messaging-platform
connections, webhook verification, user/channel pairing, delivery receipts,
and channel credentials. It converts inbound messages, scheduled triggers, and
connector events into authenticated idempotent turn requests.

It is an ingress and delivery adapter, not an agent engine and not a general
execution environment.

### Agent Service Instance

A long-lived light-agent process or replica serving compatible agent
definitions and sessions. An instance has one deployment trust boundary,
network zone, service identity, and set of provider/credential capabilities.

The current implementation has one model/provider and one optional agent
definition per instance. Supporting multiple definitions in one pool requires
request-time immutable definition resolution and a cache keyed by host and
agent definition.

### Agent Session

An authenticated conversation scope bound to:

- tenant and host;
- user or service principal;
- agent definition and version;
- memory policy and bank;
- data boundary;
- optional sandbox session;
- creation, idle, and maximum lifetime;
- optimistic version or active-turn fence.

A session ID is an opaque server-issued reference, not proof of access.

### Agent Turn

One accepted user or service request and its resulting model/tool loop. A turn
has a durable ID, idempotency key, policy snapshot, budgets, state, timestamps,
and terminal result.

### Agent Action Attempt

One effectful tool or local execution attempt within a turn. It has an
idempotency key, attempt number, lease, fencing token, approval state, result,
and reconciliation state.

Read-only remote gateway calls may use a lighter audit record, but side-effecting
or sandboxed actions require a durable attempt.

### Execution Subject

The origin-neutral identity carried by the controller/runner protocol:

    subject.kind = workflow-task | agent-turn | agent-action
    subject.id
    subject.attempt
    origin.service
    origin.instance

Workflow correlation and agent-session correlation are optional typed
extensions. They are not mandatory fields in the runner transport.

### Sandbox Session

An optional backend environment reused across related turns or actions under
one immutable policy, principal, agent definition, workspace base, and expiry.
It is separate from the chat session. Most chat sessions need no sandbox.

## Ownership

| Component | Authority |
| --- | --- |
| light-agent | Agent session, turn, model loop, memory policy, action intent, approval wait, final response |
| light-workflow | Workflow instance, workflow task, workflow retry, workflow approval, workflow transition |
| controller-rs | Runner admission, capacity, reservation, lease transport, heartbeat, quarantine |
| light-workflow-runner | Lease validation, local journal, backend lifecycle, bounded execution, cleanup evidence |
| light-agent-worker | Leased sandbox-side runtime hosting, skill/context materialization, normalized event streaming, process-tree shutdown |
| light-agent-channel | Messaging connection, webhook verification, channel/principal binding, trigger normalization, response delivery |
| ExecutionBackend | Backend-specific preparation, inspection, execution, logs, artifacts, cancellation, cleanup |
| light-gateway | API/MCP authentication, authorization, routing, network policy, and response controls |
| model provider | Model inference only; its output is untrusted input to policy enforcement |
| agent runtime adapter | One bounded model/tool loop behind the normalized runtime protocol; no authority to widen its lease |
| fixed action/service | Structured publish, signing, deploy, push, or other high-value operation |

controller-rs and the runner are origin-neutral. They do not advance an agent
turn or workflow task. The origin service reconciles the fenced result into its
own state.

## Execution Modes

Use explicit modes. Do not silently redirect one mode to another.

| Mode | Owner | Intended use | Local execution |
| --- | --- | --- | --- |
| native-workflow | light-workflow | Classification, summarization, branching, schema-bound JSON | None |
| agent-service | light-agent | Interactive chat, memory, remote model, gateway tool loop | None by default |
| runner-agent | light-agent or light-workflow | Files, shell, code, browser, local MCP, private tenant tools | ExecutionBackend |
| channel-agent | light-agent-channel plus light-agent | Messaging, scheduled triggers, personal-assistant ingress and delivery | None in channel gateway |
| fixed-action | dedicated typed service or runner template | Publish, sign, deploy, branch/PR, high-value credentials | Fixed structured operation |

### Native Workflow Agent

Keep bounded workflow reasoning in light-workflow. It receives workflow-safe
context, calls an approved remote model, validates structured output, and
returns control to explicit workflow tasks.

It receives no filesystem, local shell, dynamic tools, tenant workspace, or
release credentials.

### Agent Service

Use a long-lived light-agent service for interactive sessions:

- WebSocket or streaming chat;
- Hindsight memory;
- remote model providers;
- portal catalog caching;
- dynamic gateway tools/list and tools/call;
- independently scaled specialist agents.

The service container is an application isolation boundary, not a safe place
to execute arbitrary code. It should have no workspace mount, host container
socket, build tools, browser automation, or unrestricted local MCP server.

### Runner Agent

Use runner-agent mode when a turn needs:

- checked-out repositories or mutable files;
- shell or language runtimes;
- browser automation;
- CLI-based model agents;
- local MCP servers;
- private tenant network access;
- code generation, repair, or tests;
- untrusted tool packages or scripts.

For these cases, either:

1. keep the model loop in light-agent and lease individual local actions; or
2. place the entire model/tool loop in the sandbox when a CLI agent or
   workspace-aware model must observe and mutate local state.

The second model is required for Codex-, Pi-, and Claude Code-style execution.
Host it with `light-agent-worker`; do not start a second copy of the public
light-agent service inside the sandbox. The shared service must not spawn an
external agent CLI with its own environment.

Per-action leasing remains useful for a native service-side loop that needs one
isolated command. A workspace-aware external runtime receives one bounded
agent-turn lease, with optional policy-compatible session reuse, because its
filesystem observations, command sequence, and model context form one local
execution loop.

### Fixed Action

Publishing, signing, deployment, final tags, branch push, and pull-request
creation use fixed actions with structured inputs. They consume immutable
artifacts or an accepted canonical patch and receive a fresh scoped credential.

They do not execute arbitrary commands from the agent or mutable workspace.

## Recommended Architecture

    Portal / CLI / API                 Messaging / schedule / connector event
             |                                      |
             |                         light-agent-channel
             |                                      |
             +------------- authenticated turn ----+
                                    |
                                    v
                              light-agent
                           session and policy
                   +-------------+-------------+
                   |             |             |
                   v             v             v
            model provider  light-gateway  light-workflow
                              API / MCP       durable process
                                    |
                                    v
                              controller-rs
                                    |
                                    v
                         light-workflow-runner
                                    |
                                    v
                            ExecutionBackend
                                    |
                                    v
                       task/session sandbox
                                    |
                                    v
                         light-agent-worker
                         + runtime adapter

    Fixed high-value effects remain separate typed services/actions.

The normal interactive path does not allocate a sandbox. A sandbox is allocated
only when effective policy and the requested action require local execution.

## Product Profiles

### Enterprise Business Agent

Use the enterprise profile for API- and MCP-centered business processing.
The model loop stays in the long-lived light-agent service. The effective
catalog exposes only assigned and currently executable gateway tools. Durable
or regulated multi-step processing is delegated to light-workflow.

This profile has no workspace mount, local shell, browser process, external
agent CLI, or personal channel credential.

### Coding Agent

Use the coding profile for repository inspection, code changes, builds, tests,
local MCP, and developer tooling. The whole workspace-aware loop runs through
`light-agent-worker` in a task-scoped sandbox by default. A bounded
session-scoped workspace is an optimization that requires the same principal,
agent definition, repository base, policy, runtime adapter, backend, and
expiry.

The runtime adapter may be native or may wrap an external harness such as Pi,
Codex, or Claude Code. The adapter is selected by immutable server policy and
image/package identity, never by a prompt-supplied command. Provider access is
brokered by a runner-owned service outside the untrusted payload boundary; raw
provider keys and reusable proxy bearer tokens are not copied into the
sandbox. The worker receives only a peer-bound local channel for the current
attempt, model allowlist, data-boundary and policy digests, token/cost budget,
rate, and expiry. The generated-code process runs under a different identity
and process/mount namespace and cannot inspect, reconnect to, or inherit that
channel.

The untrusted workspace can produce a patch and diagnostic artifacts. Trusted
runner code computes the canonical diff, enforces protected paths, and exports
immutable artifacts. Branch, pull-request, push, publish, signing, and deploy
remain fixed actions over the accepted patch or commit.

### Personal Assistant

Use the personal-assistant profile for long-lived user memory, messaging
channels, proactive schedules, personal connectors, browser tasks, and
optional local-device access.

`light-agent-channel` owns platform-specific connections and principal pairing.
It does not hold model-provider or general tenant credentials. Typed remote
connectors execute through light-gateway. Browser, filesystem, desktop,
home-automation, or other user-local effects execute through a dedicated or
user-owned edge runner with an explicit capability policy.

A logical personal assistant does not require a permanent VM. A dedicated
service or runner is required when personal OAuth grants, private-network
access, legal boundaries, or local-device capabilities cannot share a service
pool safely.

Scheduled and connector-triggered turns enter the same durable per-session
queue as user prompts, carry an origin and idempotency key, and obey quiet
hours, rate, cost, approval, and notification policy. A proactive trigger
cannot interrupt an active turn or silently act as the user.

## Agent Runtime Protocol

Define a versioned `agent-runtime-protocol` shared by light-agent,
light-agent-worker, runner adapters, and test fixtures. It is separate from the
model-provider trait and from the controller/runner lease protocol.

The runtime specification includes:

- runtime adapter ID, version, immutable image/package digest, and capability
  digest;
- agent, session, turn, and execution correlation;
- bounded context and selected skill-package digests;
- workspace base, writable roots, protected paths, and change policy;
- model, tool, network, credential, approval, resource, artifact, and deadline
  policy;
- optional checkpoint/session identity and compatibility digest;
- a one-time event-stream authentication handle.

The runtime emits ordered, bounded events such as:

- `runtime.started` and `runtime.ready`;
- `model.started`, `model.delta`, `model.completed`, and usage;
- `tool.requested`, `tool.started`, `tool.result`, and `tool.failed`;
- `approval.requested` and `approval.resolved`;
- `workspace.changed` and `artifact.proposed`;
- `checkpoint.created`;
- `turn.completed`, `turn.failed`, `turn.cancelled`, or `turn.unknown`.

Each event carries the execution ID, turn/action identity, monotonically
increasing sequence, event ID, policy digest, timestamp, and bounded payload or
artifact reference. Duplicate events are idempotent. Missing sequences can be
resumed from the worker journal. An event is evidence; only light-agent can
accept it into agent-domain state.

The protocol supports start, cancel, inspect, checkpoint, resume, and bounded
input/approval responses. It does not expose a generic remote shell endpoint.

### Runtime Capabilities And Adapters

A runtime capability document declares whether the adapter supports workspace
mutation, native tools, streaming, interruption, approval suspension,
checkpoint/resume, project-local instructions, local MCP, and session reuse.
Server-owned compatibility policy maps a tested adapter version to the
capabilities it may claim.

For local execution it also carries an immutable runtime-tool manifest: stable
internal tool reference, model-facing alias, input/output schema digests,
effect class, required capability, and dispatch adapter for each shell,
filesystem, browser, or local-MCP operation. At turn admission, server policy
intersects that manifest with the execution profile and lease `allowedTools`;
where the runtime supports live enumeration, the worker intersects it again
with the current local tool set. A runtime self-report can narrow availability
but cannot add authority absent from the server-owned compatibility record.

The first adapters should be:

1. a deterministic mock adapter for protocol, recovery, and fencing tests;
2. a native bounded adapter using shared Light-Agent core logic;
3. one SDK/RPC-based coding adapter, with Pi as the preferred first candidate;
4. subprocess adapters for Codex, Claude Code, Gemini CLI, or Kilo only after
   their non-interactive event and approval contracts are pinned and tested.

Do not scrape terminal presentation output when a structured SDK, RPC, or JSON
event mode exists. Never enable a permission-bypass flag as a substitute for
the platform sandbox and approval policy.

## Agent And Workflow Interoperation

Agent and workflow orchestration are bidirectional but retain separate domain
ownership.

An agent starts a workflow through a typed gateway/API tool when a skill needs
durable branching, retries, assertions, human tasks, or long waits. The agent
stores the workflow instance reference and may stream or poll its public
status; it does not reproduce the workflow steps in its own model loop.

The existing workflow `call.agent` behavior remains the backward-compatible
`native-workflow` mode: light-workflow performs a bounded model call and
validates schema-bound JSON without an interactive session or local tools. A
future explicit `agent-service` mode submits a typed agent job to light-agent.
Light-agent may satisfy that job in its service or through runner-agent
placement according to the selected definition and policy.

light-workflow never directly launches an external agent binary and never
mutates agent session history. light-agent never advances workflow tasks.
Handoffs carry a correlation ID, caller and tenant binding, input/output
schema, deadline, idempotency key, budget, cancellation policy, and bounded
delegation depth. Cyclic delegation and unbounded agent/workflow recursion are
rejected.

## Shared Runner Contract

The workflow runner protocol should be origin-neutral before its first stable
version. Do not require processId and taskId in every wire message.

Every scheduling request and lease carries:

- execution ID;
- origin service and authenticated origin instance;
- subject kind, ID, and attempt;
- tenant and host derived from trusted identity;
- policy snapshot and digest;
- execution profile and compatibility digest;
- runner/backend selection;
- lease ID and fencing token;
- deadlines and cleanup policy;
- idempotency key;
- optional typed workflow or agent correlation.

Example standalone agent action:

    {
      "executionId": "01970f5d-2222-7000-8000-000000000001",
      "origin": {
        "service": "light-agent",
        "instance": "account-agent-east"
      },
      "subject": {
        "kind": "agent-action",
        "id": "01970f5d-2222-7000-8000-000000000020",
        "attempt": 1
      },
      "agent": {
        "sessionId": "01970f5d-2222-7000-8000-000000000010",
        "turnId": "01970f5d-2222-7000-8000-000000000011",
        "agentDefId": "01970f5d-2222-7000-8000-000000000012"
      },
      "leaseId": "01970f5d-2222-7000-8000-000000000030",
      "fencingToken": 19,
      "policyDigest": "sha256:...",
      "executionProfile": "agent-microvm",
      "commandTemplateId": "agent-tool-cargo-test",
      "deadlineAt": "2026-07-10T20:30:00Z",
      "expiresAt": "2026-07-10T20:10:30Z"
    }

The controller authenticates which services may submit each subject kind.
light-agent cannot submit workflow-task work, and light-workflow cannot mutate
an agent turn merely because both use the same runner.

### Persistence Split

Use common execution tables for controller/runner state:

- runner_scheduling_request_t;
- execution_attempt_t;
- runner session/backend capability records;
- execution session, artifact, and runtime audit records where sharing is
  appropriate.

Use origin-specific tables for domain state:

- task_info_t and workflow approval/transition records for workflow tasks;
- agent_session_t, agent_turn_t, agent_action_attempt_t, and the ordered session
  event stream for agent work.

The common attempt stores subject identity, lease, fencing, backend, normalized
result, and cleanup. The origin transaction conditionally accepts that result
and advances only its own domain object.

This split is a design decision:

- every runner-backed agent action references one shared execution_attempt_t
  row;
- execution_attempt_t contains only controller/runner concerns such as origin,
  subject, attempt, reservation, lease, fencing, runner/backend, normalized
  result, and cleanup;
- agent_action_attempt_t contains agent-domain concerns such as tool identity,
  model iteration, argument digest, effect class, approval, budgets, recovery
  policy, and acceptance into the turn;
- gateway-only actions can have an agent_action_attempt_t without an
  execution_attempt_t and instead record the gateway request/idempotency
  identity;
- controller-rs never writes agent turn, history, or approval state.

### Result-Ready Wakeup

The common execution row remains the durable source of truth. In the same
PostgreSQL transaction that conditionally stores a newly terminal
`execution_attempt_t`, controller-rs emits a versioned
`execution_result_ready_v1` notification containing only the attempt ID,
authenticated origin, subject kind, and correlation ID. It contains no result
bytes, tenant content, or authorization.

light-agent listens for the notification, loads the authoritative row, verifies
origin/subject/fencing bindings, and conditionally accepts the result in its
own domain transaction. It must also scan indexed unaccepted terminal attempts
at startup and periodically. The listener uses a dedicated connection and, on
startup or reconnect, establishes `LISTEN` before its catch-up scan so a commit
in that handoff window is either scanned or queued. `LISTEN/NOTIFY` is only a
low-latency wakeup:
delivery can be missed, duplicated, or reordered. A future typed controller
callback may provide another wakeup, but it cannot replace the authoritative
query or make controller-rs write agent tables.

## Session And Turn Model

### Session Admission

The front door authenticates the caller before accepting or resuming a session.
The server derives:

- tenant and host;
- user or service principal;
- allowed agent definition;
- model/provider and data-boundary policy;
- memory scope;
- maximum session and idle lifetime.

On new session, light-agent creates a server-issued session ID and stores the
ownership binding. On resume, all binding fields must match. A valid UUID alone
never grants access.

The existing agent_memory_bank_t.user_id and agent_def_id fields should be
populated. agent_session_history_t should either gain explicit ownership
columns or reference a new agent_session_t that contains them.

### Proposed Agent Tables

agent_session_t:

- tenant, host, session, user/service principal, and agent definition;
- definition, model, tool, memory, and execution policy digests;
- memory bank ID;
- optional execution session ID;
- state, optimistic version, active turn, created/last/idle/max expiry;
- cancellation, revocation, and retention state;
- durable execution-session cleanup request, state, and evidence correlation.

agent_turn_t:

- session and monotonically increasing turn sequence;
- origin kind such as user, channel, workflow, scheduler, or connector plus an
  immutable origin reference and bounded delegation depth;
- client message ID and idempotency key;
- immutable prompt/input reference and policy snapshot;
- model/provider reference and data boundary;
- QUEUED, RECEIVED, RUNNING_MODEL, WAITING_ACTION, RUNNING_ACTION,
  WAITING_RECONCILIATION, WAITING_APPROVAL, COMPLETED, FAILED, CANCELLED, or
  UNKNOWN state;
- enqueue sequence, queue deadline, activation time, and optional cancellation
  reason;
- token, cost, model-call, action-call, and wall-clock budgets;
- accepted result, error class, timestamps, and audit correlation.

agent_action_attempt_t:

- turn and action/tool identity;
- stable internal tool reference, model-facing alias, tool source, schema
  digest, effect classification, and selected execution placement;
- optional runtime adapter ID/version, runtime action ID, and capability
  digest;
- input schema and canonical argument digest;
- approval requirement and binding;
- numbered logical attempt and optional superseded/resumed-from attempt;
- nullable execution_attempt_id referencing the common execution_attempt_t row
  where runner-backed;
- gateway request/idempotency identity where remotely executed;
- known-success, known-failure, cancelled, or unknown outcome;
- recovery classification and remaining correction budget;
- bounded result/artifact references and reconciliation state.

agent_approval_t:

- approval ID, session, turn, canonical action intent and argument digest;
- tool/operation, destination, data-boundary and policy digests, artifact or
  patch bindings where applicable, actor authority, state, and expiry;
- source attempt when a running runtime discovered the approval boundary;
- optional execution-session approval-hold ID and bounded hold expiry;
- consuming post-approval agent-action and common execution-attempt IDs;
- a unique active approval per exact subject and single-use consumption.

agent_session_event_t, or an equivalent append-only portal event stream:

- session sequence, event ID, turn ID, optional action-attempt ID, and event
  type;
- USER_MESSAGE, MODEL_RESPONSE, ACTION_DISPATCHED, ACTION_RESULT,
  APPROVAL_REQUESTED, APPROVAL_DECIDED, TURN_TERMINAL, or SYSTEM event;
- immutable content reference/digest, source class, policy digest, timestamp,
  and actor;
- unique action-result event per accepted agent action attempt.

agent_session_history_t is a rebuildable conversation-context projection over
the ordered event stream. It is not the authoritative ledger proving that an
effectful action occurred.

Personal-assistant deployments also require channel-domain records, either in
the GenAI schema or a dedicated channel service:

- `agent_channel_binding_t` binds tenant, principal, agent, platform, channel,
  remote identity, pairing/verification state, and revocation without storing
  raw channel secrets;
- `agent_channel_delivery_t` deduplicates inbound platform events and outbound
  responses, records delivery state, and references the resulting turn;
- scheduled trigger records bind agent, session/origin, schedule, quiet-hours,
  notification, idempotency, and maximum-delay policy.

Channel records do not replace agent turns. They prove ingress and delivery;
light-agent remains authoritative for reasoning and action state.

### Turn State Machine

    QUEUED -> RECEIVED -> RUNNING_MODEL
    RUNNING_MODEL -- no tool --> COMPLETED
    RUNNING_MODEL -- tool --> WAITING_ACTION
    WAITING_ACTION -- no approval --> RUNNING_ACTION
    WAITING_ACTION -- approval required --> WAITING_APPROVAL
    WAITING_APPROVAL -- approved; allocate new attempts --> RUNNING_ACTION
    WAITING_APPROVAL -- rejected --> RUNNING_MODEL or FAILED by policy
    RUNNING_ACTION -- known success/recoverable failure --> RUNNING_MODEL
    RUNNING_ACTION -- uncertain outcome --> WAITING_RECONCILIATION
    WAITING_RECONCILIATION -- known recoverable result --> RUNNING_MODEL
    WAITING_RECONCILIATION -- terminal/unsafe --> FAILED
    WAITING_RECONCILIATION -- cannot determine --> UNKNOWN

    Any active state -- accepted cancellation --> CANCELLED
    Policy/security violation or exhausted hard budget --> FAILED

A model call may be safely retried only when it has no external effect or the
provider request is idempotent. An action with an unknown outcome is inspected
or reconciled before another attempt.

An action failure does not automatically fail the turn. A known, policy-allowed
recoverable failure such as a compiler error, failed test, linter result, or
non-zero diagnostic command is persisted as untrusted tool output and returned
to RUNNING_MODEL when correction budgets remain. The model may explain the
failure or propose a new action.

The turn becomes FAILED or CANCELLED when:

- policy, authentication, schema, or security enforcement rejects the action;
- approval is rejected and policy treats rejection as terminal;
- a hard turn deadline or token/cost/action budget is exhausted;
- the client or control plane cancels the turn;
- the action is classified non-recoverable;
- an unknown side effect cannot be reconciled and policy requires termination.

A correction is a new action identity. Reusing an attempt is allowed only when
the backend/external operation has a proven idempotency or inspection contract.
maxCorrectionActions and per-tool retry limits prevent an agent from looping on
the same failure. A turn may still finish COMPLETED with a user-facing
explanation that one or more actions failed; completion means the response was
durably delivered, not that every action succeeded.

WAITING_APPROVAL is durable agent state. It always ends the current **action
lease**, model-broker capability, and action credential. A task-scoped sandbox
is cleaned after its required evidence is exported.

A reusable `agent-session` workspace has a separate lifecycle. Under an
explicit bounded non-secret retention policy, its `execution_session_t` may
enter `IDLE_APPROVAL_HOLD` with no executable action, tool credential, or model
channel. The hold expires at the earliest of approval expiry, session idle/max
expiry, cost/retention policy, broker/credential boundary, and backend-native
TTL. Pause or a verified checkpoint is preferred over consuming active compute.
Absence of an action lease is not by itself a session-cleanup signal.

The origin may renew the session-retention record only from authenticated
session activity and never beyond the fixed maximum; it must not fake action
lease heartbeats while a person decides. If the backend cannot safely retain or
checkpoint the non-secret workspace, the runner exports an approved immutable
patch/checkpoint and cleans the sandbox. Preserving important uncommitted work
must not depend only on a live sandbox.

The origin transition into `WAITING_APPROVAL` atomically persists exactly one
session disposition: cleanup, or a policy-valid bounded hold. If common session
state later lives in another database, use an idempotent transactional outbox.
There must be no interval where a session reaper can interpret the ended action
lease as abandonment before the hold is durable.

Approval never reactivates an execution attempt. If policy knows approval is
required before dispatch, light-agent records the bound action intent and
approval but creates no common execution attempt. If a running runtime
discovers an approval boundary, it returns a known `approval_required`
terminal result; controller-rs ends the action lease and the runner revokes
grants and cleans or checkpoints according to policy. After approval,
light-agent consumes
the approval into a new numbered agent action attempt and a new common
execution attempt with a fresh lease and monotonic fencing token. The previous
attempt and its backend handle, grants, and fencing token remain immutable and
cannot resume execution. A retained session workspace is reused only after
principal/base/policy/runtime/expiry compatibility and cleanup state are
revalidated; otherwise the new action starts in a fresh sandbox and restores
only a verified policy-permitted checkpoint or patch.

### Concurrency

Only one mutating turn should own a session version at a time by default.
Multiple WebSockets or replicas must not overwrite the same history.

The default user experience is a bounded durable server-side FIFO per session,
not immediate rejection:

1. Authenticate and authorize the prompt, deduplicate its client message ID,
   assign the next session enqueue sequence, and persist a QUEUED turn.
2. Return the turn ID, state, queue position, and estimated/retry timing to the
   client. The UI may disable or label the composer, but correctness does not
   depend on client-side serialization.
3. When no active mutating turn exists, conditionally acquire the session
   version, revalidate revocation and current policy, snapshot the effective
   turn policy, and activate the oldest non-expired queued turn.
4. Allow a user to cancel a queued turn. Interrupting an active turn requires
   an explicit cancel-and-enqueue operation; a second prompt never implicitly
   cancels in-flight work.

A queued prompt is durable but is not added to the active turn's model context
or mutable history projection. It becomes eligible for conversation context
only after it wins FIFO activation, so a later prompt cannot change the meaning
of an in-flight action.

Queue depth and wait time are bounded per tenant, principal, agent, and session.
A full queue returns a retryable admission response such as 429 with
retryAfter. A 409 is reserved for a stale explicit session version or an
operation that semantically requires exclusive ownership; it is not the normal
second-prompt response.

Use a conditional active-turn or aggregate-version update across replicas. A
read-only secondary view can stream state, but it cannot bypass the FIFO or
append another active user turn without winning session activation.

## Agent Definition And Policy Snapshot

Resolve and snapshot at turn admission:

- agent definition/version;
- system instructions and selected skills;
- model/provider and regional/data-boundary policy;
- memory scope and retention;
- permitted catalog and tool policy;
- action execution profile;
- network and credential profile;
- turn/model/tool/token/cost limits;
- approval rules;
- protected workspace policy;
- artifact and audit policy.

Do not execute a long turn from mutable current rows. A catalog refresh may
narrow executable tools immediately for emergency revocation, but it cannot
widen the accepted snapshot without a new authorization decision.

For future multi-agent pooling, cache by host, agent definition ID, version,
and policy digest. Never use one global catalog entry across definitions.

## Centralized Skills Across Profiles

The centralized registry is the source of assigned skill identity, version,
instructions, taxonomy, tool/workflow links, runtime compatibility, and
governance metadata. It is not the process that executes a skill.

At turn admission, light-agent resolves an immutable effective skill set and
records every selected version and digest. A profile-specific materializer
then produces only the inputs required by the selected runtime:

| Profile/runtime | Materialized form |
| --- | --- |
| Enterprise agent | Bounded prompt instructions plus selected gateway tool schemas |
| Native workflow agent | Bounded instructions, structured input, and required output schema |
| Coding runtime | Read-only `SKILL.md`, references, and signed script/assets package inside the sandbox |
| Personal assistant | Instructions, connector/tool mappings, schedule/notification constraints, and optional reviewed package |
| Workflow-backed skill | Instructions plus a typed workflow reference and start contract |

Skill content is layered in decreasing authority:

1. server and execution policy;
2. signed platform/tenant skill versions assigned to the agent;
3. reviewed user-specific skill configuration;
4. repository or workspace-local instructions;
5. user prompts, retrieved content, and tool output.

Lower layers cannot override higher-layer policy. Repository instructions and
downloaded or generated skills are untrusted content even when useful to the
model. A self-generated skill is stored as an inactive proposal and requires
validation, scanning, review, immutable packaging, and an explicit assignment
before another turn can load it.

Do not execute source code copied directly from a mutable database row. Script
or binary content belongs in an immutable artifact with digest, provenance,
scanner results, entrypoint metadata, and a required sandbox profile. The
trusted runner—not `light-agent-worker` or generated code—downloads the
selected immutable packages before sandbox creation, verifies their
digest/signature/size and archive safety, and stages them as read-only mounts
with `nodev`, `nosuid`, and `noexec` unless a reviewed profile requires an
executable entrypoint. The worker revalidates the mounted manifest before use.
Neither the worker nor payload receives artifact-store credentials or package
download egress.

See [Centralized Skills](centralized-agent-skills.md) for the catalog and
package model and [Skill Workflow Orchestration](skill-workflow-orchestration.md)
for workflow-backed skills.

## Tool Authorization And Execution

Treat model tool calls as untrusted requests.

For each model iteration:

1. Resolve the effective catalog for the authenticated agent and turn.
2. Apply lifecycle, sensitivity, effect, approval, tenant, cost, and network
   policy.
3. Partition candidates by the server-owned execution placement recorded in
   the catalog/policy snapshot: gateway, runner, workflow, or fixed service.
4. For gateway candidates, intersect with live gateway `tools/list` and
   `toolsListAccessControl` under the downscoped turn identity.
5. For runner candidates, intersect with the execution profile, lease
   `allowedTools`, server-approved runtime-tool manifest, and live worker or
   sandbox-local MCP enumeration where supported. Do not require these tools
   to exist in gateway `tools/list`.
6. Expose workflow and fixed-service candidates only through their typed
   contracts; they are never converted to free-form local or gateway tools.
7. Form a collision-free union. Bind each model-facing name to its internal
   tool reference, placement, schema digest, and policy snapshot. A duplicate
   alias across placements fails closed unless server policy assigned distinct
   deterministic aliases.
8. Send only that accepted set to the model.
9. On returned tool call, recheck that the exact bound tool remains in the accepted
   set.
10. Parse arguments strictly. Malformed JSON fails; it does not become an empty
   object.
11. Validate arguments against the accepted input schema and routing metadata.
12. Re-evaluate effect, approval, quotas, cancellation, policy revocation, and
   destination immediately before dispatch.
13. Compute the effective delegation as the intersection of caller authority,
   agent-definition policy, turn/action policy, tool policy, and current
   revocation state.
14. For gateway execution, exchange the caller identity for a short-lived
    downscoped gateway token bound to the turn or exact action.
15. Create a durable attempt and idempotency key when the action can have an
    effect.
16. Dispatch only through the placement bound at disclosure; model output
    cannot change the route.
17. Bound, redact, classify, and persist the result before giving it back to
    the model.

The gateway remains the final API authorization and routing boundary. Agent
catalog policy is an additional restriction and must not be bypassed merely
because the gateway would accept a broader caller token.

The runner lease and runtime-tool manifest are the corresponding final local
availability boundaries. A local tool name is not authority by itself, and the
model broker, credential broker, runner control socket, and backend lifecycle
API are never included in the tool union.

### Gateway Delegation

Production agent calls do not forward the caller's full bearer token directly
to light-gateway. light-agent uses a trusted token-exchange or credential-broker
service to mint a signed, short-lived delegated token whose authority can only
narrow the caller.

The effective authority is:

    caller grants
      intersect agent-definition policy
      intersect turn/action policy
      intersect tool and data-boundary policy
      intersect current revocation and quota state

A tools/list token is scoped to the turn and accepted tool set. A tools/call
token should be scoped to one action and include or cryptographically bind:

- gateway audience;
- tenant, host, caller subject, and light-agent actor identity;
- agent definition, session, turn, and action IDs;
- exact tool or narrowly bounded tool set;
- allowed scopes, destination/service, sensitivity ceiling, and data boundary;
- policy snapshot/digest and argument or request digest where practical;
- issued-at, short expiry, unique token ID, and replay/idempotency binding.

light-gateway validates the signature, audience, expiry, actor/delegation chain,
policy binding, tool, destination, and current authorization. It intersects the
delegated token with its own access-control and tool metadata; possession of a
more powerful original user token cannot widen an agent turn.

If token exchange is unavailable or a requested binding cannot be enforced,
the production call fails closed. Direct forwarding may exist only as an
explicit local-development compatibility mode and must never be the default for
effectful or sensitive tools.

### Placement

| Tool/action | Default placement |
| --- | --- |
| Remote read-only HTTP/MCP | light-gateway |
| Remote effectful HTTP/MCP | light-gateway plus durable action attempt and approval/idempotency |
| Local command or language runtime | runner ExecutionBackend |
| Filesystem or repository mutation | runner task/session sandbox |
| Browser automation | runner sandbox with network policy |
| Local MCP server | runner sandbox or dedicated tenant service |
| Branch/PR creation | fixed action over accepted patch |
| Publish/sign/deploy | fixed external service or dedicated fixed runner action |

### Tool Results

Tool output is untrusted content even when the tool is authorized.

Result handling distinguishes action outcome from turn outcome:

- known success is persisted and normally returns to RUNNING_MODEL;
- known recoverable failure is persisted with bounded diagnostics and returns
  to RUNNING_MODEL when correction policy and budgets allow;
- known terminal failure ends or cancels the turn according to policy;
- unknown outcome enters WAITING_RECONCILIATION and cannot be represented to
  the model as if the action definitely failed;
- a new corrective tool call receives a new action ID and idempotency decision.

- enforce byte, item, nesting, and token limits;
- preserve truncation markers and full artifact references when policy permits;
- separate tool data from system instructions;
- do not follow instructions found in tool output unless the agent policy
  explicitly treats that source as instructions;
- redact secrets before persistence and again before model context;
- store the action ID, tool/version, argument digest, authorization decision,
  destination, result digest, and model iteration.

## Model Provider Boundary

### Remote API Providers

Remote API providers can run from the long-lived service when:

- the service data boundary permits the prompt;
- provider credentials are service-owned or tenant-approved;
- no local executable is spawned;
- the turn has token, cost, timeout, and concurrency limits;
- response and tool calls are treated as untrusted.

Do not send tenant-local repositories, private logs, or private-network data to
a SaaS provider unless the effective policy authorizes that transfer.

### CLI And External Agent Runtimes

Codex, Pi, Claude Code, Gemini CLI, Kilo CLI, and similar harnesses are agent
runtimes, not ordinary model API adapters. Existing CLI implementations under
`model-provider` are compatibility code and should migrate behind the
agent-runtime adapter boundary.

They run under `light-agent-worker` with runner-agent placement and require:

- fresh task or bounded session sandbox;
- minimal allowlisted environment;
- no inherited portal token, database URL, unrelated provider keys, or
  controller credential;
- explicit workspace, network, tool, and resource policy;
- local deadline and process-tree cancellation;
- bounded stdout/stderr;
- immutable binary/image identity and capability digest;
- structured SDK, RPC, or JSON event integration where available;
- normalized approval, cancellation, usage, patch, and terminal events;
- cleanup journal and backend-native expiry.

Model access for these runtimes terminates at a runner-owned broker. Prefer a
preconnected descriptor, peer-credential-checked Unix-domain socket, vsock, or
an equivalent backend-local transport. A socket pathname alone is not an
authorization boundary: the broker authenticates the attempt and peer, and
the runner prevents descriptor inheritance, cross-process `/proc` inspection,
and ptrace. The broker independently enforces the approved model, data
boundary, policy digest, token/cost budget, rate, cancellation, and expiry.
An adapter that can operate only with an extractable provider key is ineligible
for an untrusted coding profile.

Permission-bypass flags are prohibited. If an adapter needs an unattended
mode, the platform sandbox and approval policy—not a CLI bypass option—provide
the effective boundary. The shared service never invokes these binaries
directly, and light-workflow never invokes them at all.

## Sandbox Scope And Backend

Backend and session scope are separate decisions.

| Scope | Use | Default |
| --- | --- | --- |
| none | Remote model plus gateway-only tools | Long-lived agent service |
| turn/task | CLI agent, untrusted tool, one repair/action | Preferred strong isolation |
| agent-session | Interactive coding workspace reused across turns | Explicit TTL, same principal/policy/base |
| dedicated | Privileged, regulated, or long-running tenant agent | Dedicated VM or service |

| Workload | Minimum boundary | Candidate |
| --- | --- | --- |
| Bounded remote reasoning | service container | light-agent pod |
| Trusted internal command | shared-kernel-container | Rootless OCI or ordinary Kubernetes Job |
| Autonomous code or untrusted package | microvm | Cube Sandbox or Docker Sandboxes |
| Strong tenant isolation | dedicated-vm | Approved dedicated VM |
| Trusted local developer helper | host-integrated | Toolbx, never represented as a sandbox |
| Publish/sign/deploy | external-service | Fixed typed action or service |

The deployment advertises available backends. Server-owned policy chooses an
eligible backend or defers/denies execution. It never silently downgrades.

### Session Reuse

An agent sandbox session can be reused only when all of these match:

- tenant, host, principal, agent definition, and policy digest;
- workspace base revision and change policy;
- backend, template/image, and compatibility digest;
- network, credential, model-provider, and tool policy;
- maximum lifetime, idle timeout, and cleanup state.

Credentials remain task-scoped even when the workspace is reused. A session
that received a high-value credential is destroyed after the action unless an
explicit policy proves safe cleanup.

The effective physical-session expiry is the earliest of the agent session's
idle/max expiry, execution-session policy, broker/grant expiry, and
backend-native TTL. An approval hold can preserve a compatible non-secret
workspace only until that same effective expiry; it does not refresh or extend
the fixed maximum and it carries no action credential or model channel.

When light-agent closes, revokes, or expires a logical
session, the same durable transaction creates an idempotent common
execution-session cleanup request. controller-rs immediately fences and
cancels active attempts and dispatches cleanup; the runner destroys the
backend session and records evidence. Cleanup is retried across restarts and
the backend TTL remains only a final fail-safe, not the expected reclamation
path. An `EXPIRED` agent session whose physical sandbox is merely waiting for
its independent TTL is a reconciliation defect.

## Memory Boundary

Conversation history and distilled memory are domain state, not sandbox state.
The sandbox may receive a bounded prompt/context projection, but it does not
own the memory database.

Required controls:

- bind memory bank and session history to authenticated host, user/principal,
  and agent definition;
- authorize every resume, recall, retain, and history update;
- apply optimistic versioning to conversation projections;
- keep accepted action attempts and append-only session events authoritative
  over the mutable history projection;
- distinguish user-authored, tool-derived, model-derived, and operator
  instruction sources;
- prevent tool output or retrieved memory from becoming privileged system
  instructions;
- enforce retention, deletion, legal hold, export, and audit policy;
- redact or tokenize sensitive values before embedding or cross-boundary model
  transfer.

### History Conflict After An Effect

An optimistic history conflict must never cause an effectful action to be
forgotten or repeated.

When an action reaches a known terminal result, light-agent performs an
idempotent origin-acceptance transaction that:

1. conditionally accepts the current agent_action_attempt_t;
2. records or references the common execution_attempt_t/gateway result;
3. appends one ACTION_RESULT event with the action/result digest;
4. advances the agent turn to RUNNING_MODEL, WAITING_RECONCILIATION, or a
   terminal state.

Updating agent_session_history_t is a projection step after that transaction.
If its expected version is stale, the projector rereads the ordered session
events, deterministically rebuilds or merges the conversation context, and
retries the projection. It does not redispatch the tool and does not overwrite
another accepted user message.

Until the projection catches up, clients can reconstruct the authoritative
timeline from turn/action/session events. The UI may show a temporary
history-sync state, but the accepted action and audit record remain visible.
Projection lag or conflict is an operational error, not an action retry signal.

Portal-command memory writes are the production default because they preserve
event, authorization, and audit boundaries. Direct PostgreSQL mode remains an
explicitly enabled local/development compatibility profile. Longer term,
memory recall should also use a scoped
service API so the general agent pod does not require a database password.

## Authentication And Session Security

- Authenticate before WebSocket upgrade or before accepting the first message.
- Derive tenant, host, user, and allowed agent definition from trusted claims
  and server-side mappings.
- Issue an opaque session handle or signed resume token with audience, expiry,
  principal, and agent binding.
- Never use caller-provided tenant, host, user, agent, or memory-bank IDs as
  authority.
- Rotate the session handle after privilege or policy changes.
- Revoke active sessions on user, agent, provider, or policy revocation.
- Serialize concurrent mutating prompts through the bounded durable
  per-session FIFO; only the active turn acquires the session version.
- Remove committed/default bearer tokens and rotate any token that may have
  been usable.

## Credentials

The long-lived service should contain only credentials required for its
service profile. It should not hold credentials for possible future tools.

- Prefer workload identity and brokered short-lived grants.
- Keep end-user authorization separate from the service's portal identity.
- Exchange user authorization for a signed, short-lived, audience-restricted,
  turn/action-scoped light-gateway delegation token. Do not forward the
  unrestricted user token in production.
- Never forward SaaS model credentials into tenant runner sandboxes.
- Do not replace provider keys with a reusable proxy bearer token visible to
  the worker or generated payload. Use the protected, peer-bound runner broker
  channel and enforce budget and expiry at the broker.
- Never let a CLI child inherit the service environment.
- Project action credentials after policy approval and revoke them at terminal
  state or lease loss.
- Publish/sign/deploy credentials exist only in fixed actions.
- Do not place raw secrets in prompts, session history, memory, tool arguments,
  logs, artifacts, environment snapshots, or execution journals.

## Failure And Recovery

### Service Restart

The session and turn are reconstructed from durable state. An incomplete turn
is not automatically replayed:

- RUNNING_MODEL with no action may be safely failed or retried by policy;
- RUNNING_ACTION queries the gateway idempotency record or runner attempt;
- an accepted ACTION_RESULT with stale history resumes projection/rebuild and
  never redispatches the action;
- UNKNOWN action outcome requires inspection or operator decision;
- COMPLETED result can be streamed again idempotently;
- WAITING_APPROVAL has no action compute or credentials; an explicitly retained
  session workspace remains paused/checkpointed under its independent bounded
  hold and expiry.

### Client Disconnect

Policy decides whether the turn:

- cancels immediately;
- continues to a durable result for later resume; or
- continues only through the current non-effectful model call.

The decision is recorded at admission. A disconnect does not silently broaden
the deadline.

### Runner Or Backend Disconnect

Use the same lease, fencing, journal, inspection, watchdog, native TTL, and
cleanup contract as workflow execution. light-agent accepts a result only for
the current action attempt and fencing token. Result-ready notifications only
wake reconciliation; startup and periodic scans of authoritative terminal
attempts recover any notification lost while light-agent was disconnected.

### Duplicate Client Message

The client supplies a message ID scoped to the session. A duplicate returns the
existing turn or result. It does not create a second effectful action.

## Limits And Admission

Every service profile and turn policy defines:

- maximum concurrent sessions and turns;
- maximum queued prompts per session/principal/agent and maximum queue wait;
- per-principal and per-agent quotas;
- maximum input/history/retrieval/tool-output tokens;
- maximum model calls and tool calls per turn;
- maximum correction actions and per-tool recovery attempts;
- model and total wall-clock deadlines;
- cost/token budget;
- maximum pending approval time;
- sandbox queue and runtime deadline;
- session idle and maximum lifetime;
- artifact and log limits.

Saturation is not a model or tool failure. Queue or reject before starting more
work than the service, provider, gateway, or runner can support.

## Audit And Observability

Record:

- authenticated session admission and resume;
- agent definition, model, catalog, memory, and execution-policy digests;
- turn state and idempotency decision;
- model provider/model, latency, token usage, and cost without hidden
  reasoning content;
- selected, hidden, and attempted tools;
- argument and result digests, effect class, approval, destination, and
  placement;
- runner/backend/lease/fencing identity for sandboxed work;
- cancellation, unknown outcome, retry, and reconciliation;
- memory recall/retain source classes;
- sandbox cleanup and artifact evidence.

Metrics include active sessions, turn latency/state, model/tool counts and
budgets, per-session queue depth/wait, session activation conflicts,
unauthorized resume attempts, rejected model tool names,
malformed/schema-invalid arguments, downscoped-token issuance/rejection,
recoverable action failures, unknown actions, history projection lag/conflict,
approval wait, runner queue time, runtime-event lag/gaps, adapter failures,
skill-package verification failures, channel duplicate/replay rejection,
delivery latency/failure, scheduled-turn deferral, result-wakeup/catch-up latency,
oldest unaccepted terminal attempt, model-broker denial/budget exhaustion,
session-to-sandbox expiry skew, approval-held session count/age/cost,
checkpoint/restore failures, and cleanup request latency/backlog.

## Deployment Profiles

### Shared Tenant Agent Service

One horizontally scalable deployment serves compatible enterprise and
personal-assistant reasoning sessions for one tenant or strong tenant
partition. It uses remote model APIs and gateway-only tools. It has no local
workspace or external agent runtime.

### Dedicated Agent Service

Use a separate long-lived deployment when an agent requires a distinct:

- tenant or legal boundary;
- model provider credential or regional endpoint;
- private network zone;
- service identity;
- latency/scaling profile;
- memory retention policy.

This is a deployment profile, not a requirement for every logical agent.

### Coding Agent Worker Pool

light-agent submits agent-turn or agent-action execution subjects to
controller-rs. The tenant runner selects an approved backend, creates a task-
or session-scoped environment, and starts `light-agent-worker` with the pinned
runtime adapter. Worker pools are grouped by real backend, network, workspace,
model-proxy, and data-boundary compatibility, not by logical agent name.

### Personal Assistant Channel Gateway

Deploy `light-agent-channel` separately from the reasoning service. Pool only
channels and users whose webhook exposure, credential store, retention,
regional, and delivery policies are compatible. Channel delivery can continue
while a reasoning replica restarts because accepted messages and responses are
durable.

### Personal Edge Runner

Use a dedicated user- or tenant-owned runner when a personal assistant needs a
local browser profile, desktop, filesystem, home network, or device access.
The edge runner advertises explicit capabilities and receives short-lived
leases; it is not a permanently authorized remote shell.

### Release Agent

Use a dedicated runner pool and strong sandbox/VM profile. The agent can inspect
and patch a copy-on-write workspace, but trusted fixed actions apply the
accepted patch, create a branch/PR, publish, sign, or deploy. Approval is bound
to immutable subjects and waits without an active lease.

## Implementation Plan

The detailed repository and pull-request sequence is maintained in
`implementation/light-agent/2026-07-10-LightAgentRuntimeAndProfilesImplementationPlan.md`.

### Phase 0: Harden The Current Service

- Remove and rotate embedded default bearer tokens.
- Authenticate session admission and bind host/user/agent ownership.
- Populate memory-bank ownership and reject unauthorized resume.
- Revalidate model tool names against the accepted per-turn set.
- Strictly parse and schema-validate arguments.
- Exchange caller authorization for downscoped gateway delegation tokens.
- Bound/redact tool output and retrieved memory.
- Add overall turn timeout, cancellation, action/model-call limits, and
  a bounded per-session FIFO with one active mutating turn.
- Disable CLI providers in shared-service profiles.

### Phase 1: Durable Sessions And Turns

- Add agent_session_t, agent_turn_t, agent_action_attempt_t, and the append-only
  agent session event stream/projection.
- Add message idempotency, durable FIFO sequencing, and optimistic session
  activation/versioning.
- Snapshot definition, catalog, model, memory, and execution policy.
- Persist model/action state and normalized bounded results; accept an action
  and append its ACTION_RESULT event before updating conversation history.
- Rebuild the history projection from ordered events after a version conflict.
- Add approval wait without compute.
- Distinguish action-lease release from a bounded execution-session
  `IDLE_APPROVAL_HOLD`; task sandboxes clean immediately while an eligible
  non-secret session workspace may pause/checkpoint until its independent
  effective expiry.
- Bind approvals to immutable action intents and consume approval into a new
  action/common execution attempt with fresh fencing; never reopen an earlier
  attempt.
- On session close, revocation, or expiry, atomically create an idempotent
  common execution-session cleanup request.
- Move production memory writes to portal-command mode.

### Phase 2: Origin-Neutral Runner Contract

- Add execution subject and origin types to execution-runner-protocol.
- Make controller scheduling and common execution attempts origin-neutral.
- Reference shared execution_attempt_t from runner-backed
  agent_action_attempt_t rows while keeping agent-domain fields out of the
  common table.
- Authenticate allowed subject kinds per origin service.
- Keep workflow and agent domain transitions separate.
- Emit identifiers-only `execution_result_ready_v1` PostgreSQL wakeups in the
  common terminal-result transaction. Add light-agent LISTEN handling plus
  startup and periodic indexed catch-up; notification delivery is never the
  source of truth.
- Add durable execution-session cleanup dispatch and evidence reconciliation.

### Phase 3: Agent Runtime Protocol And Worker

- Add agent-runtime-protocol, ordered runtime events, and capability documents.
- Add stable tool-source/placement identities and an immutable local
  runtime-tool manifest. Gateway candidates intersect gateway `tools/list`;
  local candidates intersect server compatibility, execution policy, lease
  `allowedTools`, and live worker/local-MCP enumeration.
- Add a deterministic mock adapter and the `light-agent-worker` executable.
- Route command, filesystem, browser, local MCP, and external agent runtimes
  through ExecutionBackend.
- Add a runner-owned model-broker transport contract with peer/attempt binding,
  no payload-visible reusable bearer, separate worker/payload identities, and
  broker-enforced model and budget policy.
- Reconcile worker events into agent-domain state without giving the worker
  direct database ownership.

### Phase 4: Skill Packages And Materialization

- Add runtime compatibility and immutable skill-package records.
- Verify digest, provenance, scan result, entrypoint, and sandbox policy before
  read-only materialization.
- Make the trusted runner download, safely extract, verify, and stage selected
  packages before sandbox creation. The worker only revalidates mounted bytes
  and has no artifact-store credential or download path.
- Implement enterprise, workflow, coding, and personal-assistant materializers.
- Treat generated and repository-local skills as untrusted proposals/content.

### Phase 5: Coding Agent Profile

- Enable Cube Sandbox for untrusted task-scoped coding turns.
- Add the first structured SDK/RPC coding adapter, preferably Pi.
- Add optional bounded workspace-session reuse.
- Add bounded approval-hold, pause/checkpoint, restore, expiry, cost, and
  cleanup semantics without renewing an action lease.
- Add protected runner-brokered model access, canonical patch export, protected
  paths, artifacts, watchdog, origin-driven session cleanup, native TTL
  backstop, and cleanup evidence.
- Migrate direct CLI model-provider implementations behind runtime adapters and
  disable their shared-service execution path.

### Phase 6: Multi-Agent Service Pooling

- Resolve an authorized agent definition per session/turn.
- Cache immutable definitions/catalogs by host, ID, version, and digest.
- Route provider/data-boundary profiles without sharing incompatible secrets.
- Scale replicas with durable session admission rather than in-memory
  affinity.

### Phase 7: Personal Assistant Profile

- Add light-agent-channel, channel/principal binding, webhook verification,
  delivery idempotency, and proactive trigger policy.
- Add typed connector tools and optional dedicated personal edge runners.
- Add quiet hours, notification policy, scheduled-turn admission, and
  connector credential brokering.

### Phase 8: Workflow Bridge And Fixed High-Value Actions

- Keep existing call.agent behavior as native-workflow by default.
- Add an explicit agent-service job mode with schema, deadline, idempotency,
  cancellation, correlation, and delegation-depth controls.
- Expose workflow start/status/cancel as typed agent tools.

- Add accepted-patch, branch/PR, publish, sign, and deploy contracts.
- Require immutable input, approval, provenance, and fresh action-scoped
  credentials.
- Consume every approval into a new numbered domain action and common execution
  attempt with a fresh lease and fencing token.
- Reuse the trusted fresh-checkout and protected-path design.
- Rebuild releases from reviewed immutable commits.

## Acceptance And Failure-Injection Tests

- A valid session cannot be resumed by another principal, agent, host, or
  tenant.
- Concurrent prompts from one or more replicas receive durable FIFO sequence;
  one turn becomes active and the rest remain queued in order.
- Queue-full admission is bounded and retryable; it does not drop or silently
  execute a prompt.
- Duplicate client delivery resolves to the existing turn and cannot create a
  second action.
- A model-returned hidden or unadvertised tool is rejected before gateway
  dispatch.
- A gateway-only tool absent from gateway `tools/list` is hidden without
  removing an independently authorized runner tool; a runner tool absent from
  the lease/runtime/local manifest is hidden even if the gateway has the same
  name.
- Model-facing alias collisions across placements fail closed, and a returned
  tool call cannot switch its snapshotted gateway/runner/workflow/fixed-service
  dispatch route.
- Malformed or schema-invalid arguments never become an empty object.
- Gateway authorization remains effective even when the catalog is stale.
- Tool and memory output cannot exceed context limits or become system
  instructions.
- A known recoverable command failure returns to the model within correction
  budgets; it does not automatically fail the turn.
- An unknown action is reconciled and cannot be repeated or described as a
  definite failure.
- A successful effect followed by a history-version conflict remains present
  in agent_action_attempt_t and the session event stream; projection recovery
  never redispatches it.
- Gateway calls use a signed token narrowed to the caller, agent, turn/action,
  tool, data boundary, policy digest, audience, and expiry; the original broad
  user token is not forwarded.
- Service restart during model-only work follows the configured retry policy.
- Restart after effectful dispatch reconciles instead of blindly repeating.
- Terminal result committed while light-agent is offline is found by indexed
  catch-up and accepted once; dropped, duplicate, and reordered notifications
  do not change correctness.
- WAITING_APPROVAL holds no active action lease, model-broker channel, or
  action credential. A task sandbox is cleaned; an eligible non-secret
  session workspace is retained only through a separately persisted bounded
  hold or verified checkpoint.
- Missing an action lease does not clean an approval-held session, while hold
  expiry, logical session close/revocation, or policy mismatch does. Approval
  cannot extend the session maximum lifetime.
- Approval creates a fresh post-approval action/common execution attempt and
  fencing token; the pre-approval attempt, handle, and grant remain unusable.
- CLI provider processes receive only an allowlisted environment inside a
  sandbox.
- Generated code cannot read a provider/proxy bearer, inspect or inherit the
  worker's model-broker channel, impersonate another attempt, choose an
  unauthorized model, or exceed broker-enforced token/cost limits.
- A CLI/provider timeout kills the process tree and triggers cleanup.
- A shared light-agent process cannot directly start an external agent runtime.
- Worker runtime events are ordered, resumable, bounded, and cannot mutate an
  agent turn without origin-side acceptance.
- A runtime adapter cannot claim an unapproved capability or select a weaker
  sandbox than the turn policy requires.
- A coding turn receives only the selected immutable skill packages and
  workspace-local instructions cannot grant additional authority.
- Package download, digest/signature mismatch, unsafe archive entries, or
  staging failure occurs before sandbox start; sandbox code has no
  artifact-store credential or package-download egress.
- A generated skill remains inactive until it is scanned, reviewed, packaged,
  and assigned.
- A sandboxed turn survives controller and runner reconnect without accepting a
  stale fencing token.
- A session-scoped workspace cannot be reused across principal, agent, base,
  policy, backend, or expiry changes.
- Closing, revoking, or expiring an agent session durably fences active work
  and reclaims its physical sandbox without waiting for backend-native TTL;
  cleanup survives controller and runner restart.
- Toolbx and ordinary containers cannot satisfy a microVM requirement.
- A spoofed channel user, replayed webhook, duplicate delivery, or scheduled
  trigger cannot create an unauthorized or duplicate turn.
- A personal channel gateway has no model-provider key, unrestricted shell, or
  tenant-wide connector credential.
- A native-workflow agent call remains backward compatible and a service-mode
  call cannot cause an unbounded agent/workflow delegation cycle.
- Publish/sign/deploy cannot be invoked as a free-form agent tool.
- Secret scanning finds no credential in prompts, history, memory, logs,
  artifacts, journal, or child environment.

## Open Decisions

- Whether the first production deployment remains one agent definition per
  service or introduces request-time multi-agent pooling immediately.
- Which session-scoped backends support safe checkpoint/restore.
- Which structured coding adapter is enabled first after the mock and native
  adapters, and which versioned SDK/RPC contract is pinned.
- Which runner-owned model broker and protected local transport are supported
  first on each backend: preconnected descriptor, peer-checked Unix-domain
  socket, vsock, or backend-native equivalent.
- Which object store, signer, scanner, and review service own immutable skill
  packages.
- Which channel bindings and personal connector grants remain in GenAI domain
  tables versus a dedicated channel/identity service.
- Which exact workflow syntax selects `agent-service` while preserving the
  existing native-workflow default.
- Which model providers support useful request idempotency.
- Whether client disconnect defaults to cancel or durable continuation.
- Which memory read service replaces direct PostgreSQL recall.
- Which agent actions require workflow-owned approval versus standalone
  agent-owned approval.

## Recommendation

Keep light-agent as the interactive session, durable turn, policy, memory, and
model orchestration service for all profiles. Do not assign infrastructure per
agent definition or fork separate enterprise, coding, and personal-assistant
engines. Deploy service pools by real trust and data boundaries.

Use the shared controller/runner/ExecutionBackend path whenever an agent needs
local execution or stronger isolation. Host workspace-aware loops in the small
sandbox-side `light-agent-worker`; host messaging connections in the separate
`light-agent-channel`; keep external agent products behind runtime adapters.
Make the runner protocol origin-neutral so workflows and standalone agent turns
share capacity, fencing, backend lifecycle, watchdog, credentials, artifacts,
and cleanup without sharing domain ownership.
