# Claude Code CLI Provider Option

Status: deferred design reminder, September 11, 2026. No provider implementation,
route enablement, or API conformance is implied. Implement and qualify the
[Claude Personal Worker](../light-agent/claude-personal-worker.md) first. This
option is a future feasibility study, not part of that worker's delivery scope.

## Purpose And Candidate Architecture

Explore an owner-only local connector that runs the official Claude CLI behind
a limited LLM Gateway API profile. The intended benefit is standard client
requests for supported text-generation tasks, while the local harness retains
its native authentication boundary.

```text
Client -> llm-gateway -> owner-bound local connector
                      -> pinned Claude CLI -> native vendor service
```

The gateway handles Light authentication, governed aliases, validation, and API
response semantics. A supervised local connector owns the process and native
login context. No subscription token is collected, copied, or forwarded by the
gateway. A shared multi-user subscription pool is outside this proposal.

The coding worker intentionally owns repository tools. This inference connector
must disable and externally constrain repository access, shell, MCP, hooks,
plugins, and other side effects. Tool denial cannot rely on a prompt. If a task
needs native repository execution or human approvals, use the coding-worker
contract instead of presenting it as ordinary inference.

## Existing Building Blocks And Transport

`crates/model-provider/src/claude_code.rs` already wraps Claude CLI, but its agent
path bypasses permissions, buffers output, and flattens conversation roles into
text. It does not establish Responses, Messages, or external-tool conformance.
Do not promote that wrapper unchanged as this connector.

Prototype bounded Tokio subprocess pipes with `-p` and structured output. A
resident process using `--input-format stream-json` is an optional optimization
after its input envelopes, per-turn outcomes, cancellation, and idle behavior
are qualified. Do not use PTY prompt matching or write `y` to approve tools.
The public [CLI reference](https://code.claude.com/docs/en/cli-reference) and
[headless guide](https://code.claude.com/docs/en/headless) are qualification inputs;
recheck the exact supported flags against the selected release.

Native `--model` selection must come from a governed alias mapping and the user's
entitlements. Unknown models fail explicitly. Subscription execution must not
silently switch to an API key or paid fallback. Configuration containment and
native login must work together; `--bare` is not a personal-login solution.

## Proposed Initial Contract

Begin with an explicitly advertised text-only subset of one public API. Merely
wrapping the CLI's final text in JSON does not implement `/responses` or
`/messages`.

- Preserve supported system/instruction and message-role semantics. Reject
  unsupported role combinations rather than serialize them into a user prompt.
- Reject client tools, images, embeddings, structured output, reasoning controls,
  storage, and any other unqualified fields before dispatch.
- Stateless Messages requests must not inherit hidden native conversation state.
  Do not combine full client history with automatic CLI resume and duplicate the
  conversation. Choose an explicit state model for each client profile.
- Responses continuation requires owner-bound public-response/native-session
  mapping, branch semantics, retention, idempotency, and crash recovery. Until
  qualified, reject `previous_response_id` and related stateful features.
- Keep request/response IDs separate from native `session_id`. Parse the pinned
  result schema; do not assume example fields such as `sessionId` or `text`.
- Qualify buffered completion first; add streaming only with correct API-specific
  event ordering, content blocks, completion reasons, usage, and cancellation.
- Bound queues, deadlines, frames, total output, retries, and process lifetime.
  Keep usage advisory and surface exhaustion without automatic paid fallback.

Returning a synthetic permission function call does not support generic client
tools: permission authorizes harness execution, whereas a tool-result message
reports client execution. A generic agent requiring function calling will need
a separately qualified external-tool bridge or an ordinary API-backed provider.

## Eligibility And Architectural Decision

Anthropic's [subscription support update](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan)
says the proposed credit-pool changes were paused. Its
[credential-use guidance](https://code.claude.com/docs/en/legal-and-compliance)
also restricts third-party subscription integration. Do not infer authorization
for this gateway from CLI technical feasibility or the token remaining local.
Record an eligibility decision for this specific use before supported deployment;
recheck current guidance rather than freezing either page's wording into policy.

The [gateway API contract](../light-gateway/llm-gateway-api.md) contains both a
potential owner-scoped native-connector allowance and an explicit CLI boundary.
A separate ADR must reconcile those statements and define any narrowly scoped
exception. This reminder does not change the accepted gateway contract.

## Qualification And Revisit Criteria

Revisit after the Claude worker is qualified and a named client needs the
supported inference subset. Prove no native tool side effects, native-login and
configuration containment, owner isolation, alias/model binding, message fidelity,
correct errors, process cleanup, cancellation, output bounds, and sanitized
telemetry. Pin the binary and fixture schemas; qualify the actual public client
API rather than just a successful native prompt.

Measure latency and memory before adopting resident processes. Their reuse must
not leak conversation or permissions between requests. Missing history or an
uncertain execution must never cause silent duplicate generation.

Related option: [Codex CLI Provider](codex-cli-provider.md).
