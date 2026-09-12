# Codex CLI Provider Option

Status: deferred design reminder, September 11, 2026. No provider implementation,
route enablement, or API conformance is implied. The immediate priority is the
[Claude Personal Worker](../light-agent/claude-personal-worker.md); revisit this
option after that worker is qualified.

## Purpose And Candidate Architecture

Explore whether an owner-only local connector can expose a deliberately limited
LLM Gateway API surface backed by the official Codex harness. This would let a
client use a standard request envelope for supported text-generation operations
without implementing native harness integration itself.

```text
Client -> llm-gateway -> owner-bound local connector
                      -> pinned Codex App Server -> native vendor service
```

Prefer the documented App Server interface of the Codex CLI over terminal
scraping or repeatedly parsing human-readable output. The connector owns the
native process and local authentication context; the gateway owns inbound Light
authentication, alias policy, request validation, and the public response format.
Native subscription credentials never enter gateway requests or credential stores.

This is separate from a coding worker. The coding worker deliberately edits and
tests repositories under a runner lease. An inference connector must have no
repository, shell, MCP, plugin, or other native execution authority. If that
restriction cannot be enforced, use a workflow coding action instead.

## Existing Building Blocks And Limits

The qualified `codex-app-server-v1` worker provides useful lifecycle, cancellation,
version-pinning, and event-parsing experience. Reuse appropriately factored code,
but issue a separate connector contract and qualification record.

`crates/model-provider/src/codex.rs` is an HTTP provider, not a Codex CLI adapter.
Its name does not establish support for this option. Do not reuse direct native
subscription-token handling as a substitute for the official harness boundary.
The App Server speaks a harness protocol; it is not itself the public Responses
API. See [Codex App Server](https://developers.openai.com/codex/app-server) and
[authentication](https://developers.openai.com/codex/auth). Recheck both before
implementation; native login support does not by itself authorize a shared
subscription-backed inference service.

## Proposed Initial Contract

Start with text-only requests, one in-flight operation per owner-scoped capacity
slot, and explicit governed aliases. Do not silently substitute models or fall
back to paid API routes. Each connector is visible only to its owner and the
owner's authorized agents, never a shared provider pool.

- Preserve supported message roles and instruction precedence; reject any
  conversion requiring lossy concatenation.
- Reject client tools, embeddings, images, reasoning controls, structured output,
  storage, or other unsupported options before invoking Codex.
- Choose one public API subset for the first probe. A Chat Completions response
  envelope cannot stand in for Responses or Anthropic Messages conformance.
- Generate a unique public response ID for every operation, distinct from the
  native thread ID. Start stateless; reject `previous_response_id` until owned
  state, branch, retention, retry, and replay semantics are qualified.
- Map streaming only after item ordering, deltas, terminal outcomes, usage, and
  cancellation pass client-specific fixtures. Otherwise advertise buffered only.
- Record native usage as advisory. Do not invent billable cost, remaining quota,
  or subscription entitlement from token counts.

Function calling is a separate milestone. A native approval request authorizes
Codex to execute a tool; it is not equivalent to returning a function invocation
for the client to execute. Do not fabricate compatibility by translating an
approval into a generic `request_permission` tool call.

## Decisions And Gates Before Work Starts

The [gateway API contract](../light-gateway/llm-gateway-api.md) mentions potential
owner-scoped native connectors but explicitly excludes CLI credential caches and
places personal CLI automation outside the gateway. Resolve that architectural
boundary in an ADR before implementing this option; this note does not amend it.

Required gates are vendor eligibility for the intended use, enforced absence of
native tool authority, pinned binary/protocol compatibility, owner isolation,
message fidelity, public API subset conformance, bounded output, cancellation,
process-tree cleanup, secret-free audit evidence, and quota/error behavior.
Live qualification must use the intended authentication class and must not treat
skipped subscription tests as passing evidence.

Open questions: which client actually needs this subset, whether its agent loop
requires function calling, whether native instruction semantics preserve that
client's contract, and whether startup overhead justifies a resident connector.
A resident process requires resource limits and idle eviction; it must not create
hidden conversational history for otherwise stateless requests.

Related option: [Claude Code CLI Provider](claude-code-cli-provider.md).
