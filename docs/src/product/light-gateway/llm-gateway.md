# LLM Gateway Design

## Status

Proposal.

## Decision Summary

Add an LLM inference handler to `light-gateway` with these initial decisions:

- Expose an OpenAI-compatible client API. Implement `GET /v1/models` and
  `POST /v1/chat/completions` first, including Server-Sent Events (SSE)
  streaming. Add `POST /v1/responses` after the provider abstraction can
  preserve its richer content and event model.
- Treat the request `model` as a public, governed model alias. Clients do not
  select provider credentials, provider base URLs, or physical deployments.
- Keep the wire protocol separate from the provider abstraction. The gateway
  translates OpenAI-compatible requests into a provider-neutral internal
  representation and translates normalized provider results back to the
  selected public protocol.
- Reuse `crates/model-provider` implementations, but do not expose the current
  `Provider` trait directly as the HTTP contract. It needs typed errors,
  streaming events, structured content blocks, cancellation, richer request
  options, and per-model capabilities before it is a production gateway
  boundary.
- Reuse the existing Light handler chain for correlation, authentication,
  authorization, request rate limits, metrics, and common traffic policy. Add
  LLM-specific routing, token and cost budgets, provider health, and usage
  accounting in a dedicated runtime.
- Keep LLM inference and MCP tool execution as separate protocol boundaries.
  The LLM gateway can accept tool definitions and return tool calls, but the
  client agent remains responsible for executing those calls through the MCP
  router and returning tool results to the model.
- Keep configuration and administration in the Light control plane. Do not add
  a second gateway-specific administration UI or public mutation API in the
  first implementation.

## Context

`light-gateway` already has most of the surrounding gateway capabilities:

- Pingora HTTP and HTTPS listeners and proxy transport.
- Ordered handler chains configured by `handler.yml`.
- JWT, API key, basic, unified-security, and agent-delegation authentication.
- Access control, request rate limits, correlation, metrics, headers, and CORS.
- MCP request handling through the `mcp` application handler.
- Browser-to-agent WebSocket routing through the `websocket` traffic handler.
- Config registration, config-server bootstrap, atomic `ConfigManager` swaps,
  and reloadable modules.

`crates/model-provider` already contains provider clients for OpenAI, Azure
OpenAI, Anthropic, Bedrock, Gemini, GLM, Ollama, OpenRouter, Telnyx, and generic
OpenAI-compatible endpoints. It also contains account- or CLI-oriented clients
such as Codex, Copilot, Claude Code, Gemini CLI, and Kilo CLI, plus two wrapper
providers:

- `RouterProvider` resolves a `hint:<name>` to a configured provider and model.
- `ReliableProvider` performs retries and walks provider/model fallback chains.

The current common types are intentionally small and agent-oriented:

- `ChatMessage` has a string role and string content.
- `ChatRequest` has messages and optional tools.
- `ChatResponse` is buffered text, tool calls, usage, and optional reasoning
  content.
- `ProviderCapabilities` contains only native tool calling, vision, and prompt
  caching flags.
- Provider errors are returned as `anyhow::Error`.

That is enough for the current `light-agent` and `light-workflow` call paths,
but it cannot faithfully implement a public LLM gateway. For example, it has no
common incremental stream, typed provider status and `Retry-After`, structured
multimodal content blocks, response-format contract, cancellation signal, or
per-operation capability declaration.

Two open source gateways provide useful feature signals:

- [Bifrost](https://github.com/maximhq/bifrost) emphasizes an OpenAI-compatible
  API, provider-native compatibility adapters, retry and fallback, weighted
  routing, virtual-key governance, hierarchical budgets, semantic caching,
  plugins, observability, and MCP integration.
- [LiteLLM](https://github.com/BerriAI/litellm) exposes OpenAI-format and native
  endpoints across many providers and adds proxy authentication, virtual keys,
  spend tracking, rate limits, routing, fallback, caching, guardrails, and
  logging.

The Light design should adopt the durable product capabilities without copying
either project's control plane. Light already has a portal, config server,
controller, security handlers, access control, and an MCP router.

## Goals

- Give applications and agents one stable base URL and one common API across
  supported model providers.
- Let existing OpenAI SDK users migrate by changing the base URL and client
  credential rather than rewriting request and response handling.
- Support buffered and streaming chat, tool calling, structured output, and
  supported multimodal input without losing provider semantics silently.
- Route public model aliases to one or more physical provider deployments.
- Provide retry, fallback, load balancing, circuit breaking, health-aware
  routing, and cancellation with well-defined streaming behavior.
- Enforce model access, data-boundary constraints, token limits, cost budgets,
  concurrency limits, and request rate limits per authenticated identity.
- Record normalized usage, cost, latency, time to first token, route decisions,
  retry/fallback activity, and policy outcomes.
- Protect provider credentials and prevent clients from choosing arbitrary
  upstream URLs or passing provider secrets through the gateway.
- Reload provider, alias, route, and policy snapshots atomically without
  interrupting in-flight requests.
- Make provider conformance measurable so an alias is offered only when every
  eligible target can satisfy its declared capabilities.

## Non-Goals

- Do not run an autonomous agent loop in the LLM gateway. It does not execute
  model-returned tool calls or decide when an agent task is complete.
- Do not replace the MCP router or merge MCP JSON-RPC with the LLM HTTP API.
- Do not proxy arbitrary client-supplied provider base URLs, API keys, or cloud
  credentials.
- Do not expose every provider-specific option through the common API.
  Provider-native compatibility endpoints can be added deliberately when a
  real client need justifies their maintenance cost.
- Do not support fine-tuning, training, file storage, assistants, or durable
  conversation state in the first implementation.
- Do not enable account- or CLI-oriented providers in a shared gateway until
  their credential isolation, licensing, concurrency, and multi-tenant behavior
  have passed a separate security review.
- Do not log prompts, completions, images, tool arguments, or reasoning content
  by default.
- Do not promise identical model output after fallback. Fallback preserves the
  API and required capabilities, not model behavior.

## Reference Feature Comparison

The comparison is a requirements input, not a compatibility promise.

| Capability | Bifrost signal | LiteLLM signal | Light direction |
|------------|----------------|----------------|-----------------|
| Common inference API | OpenAI-compatible API plus provider SDK adapters. | OpenAI input/output format plus native endpoints. | OpenAI-compatible API first; provider-native adapters only when justified. |
| Provider abstraction | Many hosted and local providers. | Broad provider and endpoint coverage. | Reuse `model-provider`, gated by per-operation conformance tests. |
| Routing | Provider/model/key routing and weighted strategies. | Deployment router and load-balancing strategies. | Public alias to eligible deployment targets with weighted, health-, policy-, and capability-aware selection. |
| Reliability | Retries, key rotation, and sequential fallbacks. | Retries, cooldowns, and cross-deployment fallback. | Typed retry classes, `Retry-After`, circuit breakers, and no fallback after visible stream output. |
| Tenant credentials | Virtual keys. | Virtual keys and proxy keys. | Reuse Light authentication; map the authenticated client, user, agent, and host to an LLM policy. |
| Cost governance | Hierarchical budgets and rate limits. | Spend tracking and budgets by several scopes. | Atomic token/cost reservation and usage reconciliation by configured Light policy scopes. |
| Caching | Exact/provider and semantic caching. | Configurable response caches. | Exact cache later; semantic cache is opt-in and tenant/policy isolated. |
| Guardrails | Plugin-based request and response controls. | Per-project guardrails and callbacks. | Reuse handler-chain policy where possible and add streaming-aware LLM policy hooks. |
| Observability | Metrics, tracing, and request logging. | Logging callbacks, usage, cost, and latency. | Existing correlation/metrics plus bounded-cardinality LLM metrics and OpenTelemetry events. |
| MCP | MCP gateway and tool filtering. | MCP support and model-tool integration. | Keep the existing MCP router authoritative for tool discovery and execution. |
| Administration | Built-in configuration and monitoring UI. | Admin UI and APIs. | Use Light Portal, config server, and controller instead of adding another control plane. |

## Common Client API

### API Choice

Use the OpenAI API shape as the public compatibility profile.

| Option | Strength | Limitation | Decision |
|--------|----------|------------|----------|
| OpenAI Chat Completions | Widest existing SDK and agent-framework compatibility; maps closely to current provider clients; supports SSE and tool calls. | Its message model is less expressive than newer event/item APIs. | MVP. |
| OpenAI Responses | Better fit for reasoning models, structured content items, and richer streaming events. | Requires a significantly richer internal contract and has less uniform third-party coverage. | Phase 2, built on the same canonical internal types. |
| Provider-native APIs | Maximum fidelity for one provider and drop-in support for provider SDKs. | Multiplies codecs, tests, and long-term compatibility obligations. | Add selected adapters later; not the canonical client API. |
| Light-specific inference API | Full control over versioning and semantics. | Requires new SDKs and creates avoidable client migration work. | Do not use as the primary external API. |

The OpenAI-compatible contract is a compatibility profile, not a claim that
every provider supports every OpenAI option. Capability checks and explicit
errors are part of the contract.

### Endpoint Roadmap

| Endpoint | Priority | Notes |
|----------|----------|-------|
| `GET /v1/models` | MVP | Return only public aliases authorized for the caller. Do not enumerate raw provider deployments. |
| `POST /v1/chat/completions` | MVP | Buffered and SSE streaming; text, tool calls, supported image input, and structured output where the alias declares support. |
| `POST /v1/responses` | Phase 2 | Use native response items and events rather than flattening through Chat Completions. |
| `POST /v1/embeddings` | Phase 2 | Add only after an embedding operation exists in the canonical provider trait and pricing model. |
| `POST /v1/moderations` | Phase 2 or policy service | Decide whether this is a provider operation or a Light policy endpoint before implementation. |
| Images, audio, rerank, batches, and files | Later | Each needs its own capability, size, cost, storage, streaming, and retention contract. |
| Realtime WebSocket/WebRTC | Later | This is provider realtime inference, not the existing UI-to-agent WebSocket router. Implement it as a separate protocol handler. |

### Authentication And Headers

No Light-specific header is required for a normal SDK call.

- `Authorization: Bearer <credential>` carries a Light-issued API key, JWT, or
  agent-delegation credential accepted by the configured handler chain. It is
  never a provider API key.
- `Content-Type: application/json` is required for JSON request endpoints.
- `X-Correlation-Id` and `X-Traceability-Id` use the existing Light correlation
  contract.
- `X-Light-Session-Id` is an optional routing hint for session stickiness. It
  must be bounded, treated as untrusted input, and scoped by authenticated
  principal so two tenants cannot collide.
- `Idempotency-Key` can enable request deduplication where the selected
  operation and storage policy support it. It cannot guarantee that a provider
  did not bill a timed-out upstream attempt.
- `x-request-id` should be returned for OpenAI SDK diagnostics and linked to the
  Light correlation and trace identifiers in server-side telemetry.
- Provider name, physical model, base URL, key ID, raw error body, and internal
  policy details are not returned by default. Authorized diagnostic tooling can
  retrieve them from audit events.

The OpenAI `user` field is optional attribution metadata. It does not establish
identity and cannot override the authenticated principal.

### Public Model Names

The request `model` is a logical alias such as `chat-fast-v1`,
`reasoning-standard-v1`, or `private-code-v2`.

An alias defines:

- Allowed operations and request features.
- Maximum input and output sizes.
- Data classification and residency requirements.
- Eligible provider deployments and physical model identifiers.
- Routing, retry, fallback, timeout, and budget policy.
- Pricing policy and capability snapshot version.
- Deprecation and replacement metadata.

Provider-prefixed names such as `openai/gpt-x` can be convenient for local
development, but public production policy should disable them. Otherwise the
client can bypass alias-level routing, residency, and lifecycle controls.

`GET /v1/models` returns only aliases visible to the caller. A separate
authenticated control-plane view can show target deployments and detailed
capabilities.

### Chat Completions Compatibility Profile

The MVP should support these fields when the selected alias declares the
required capability:

- `model`
- `messages` with text content and supported `image_url` content parts
- `temperature` and `top_p`
- `max_tokens` and `max_completion_tokens`, normalized to one internal output
  limit with a conflict error if both disagree
- `stop`
- `stream` and `stream_options.include_usage`
- `tools`, `tool_choice`, and `parallel_tool_calls`
- `response_format` for text, JSON object, and JSON Schema where supported
- `user` and bounded metadata for attribution

The gateway must not silently drop a non-default option. The default
`unsupportedParameterPolicy` is `reject`. A per-alias allowlist can permit
provider-specific pass-through fields only when every eligible route handles
them consistently. Unknown null or SDK-default fields can be ignored if the
compatibility profile explicitly documents them.

Reasoning summaries may be exposed only through a documented public field or
Responses event. Hidden chain-of-thought or raw provider reasoning content must
not be logged or returned merely because a provider client captured it.

Example:

```sh
curl https://gateway.example.com/v1/chat/completions \
  -H 'Authorization: Bearer <light-credential>' \
  -H 'Content-Type: application/json' \
  -H 'X-Correlation-Id: example-request-1' \
  -d '{
    "model": "chat-fast-v1",
    "messages": [
      {"role": "user", "content": "Summarize the attached incident."}
    ],
    "temperature": 0.2,
    "stream": true
  }'
```

### Streaming Contract

Chat Completions streaming uses `text/event-stream`, OpenAI-compatible
`chat.completion.chunk` data frames, and a terminal `data: [DONE]` frame.

Streaming changes reliability semantics:

1. Before the gateway emits the first semantic output event, it may retry or
   choose an eligible fallback according to policy.
2. After any text, tool-call argument, or other semantic output is visible to
   the client, the gateway must not retry or switch providers. Doing so can
   duplicate text, corrupt incremental JSON arguments, or create a second tool
   call.
3. A failure after streaming starts emits a sanitized error event when the
   compatibility profile permits it, then closes the stream without `[DONE]`.
4. A downstream disconnect cancels the provider request promptly and records
   the final known usage. Cancellation is best effort because a provider can
   continue billing work already accepted upstream.
5. Time to first token, stream duration, client cancellation, and upstream
   cancellation outcome are recorded separately.

The current MCP stream writer demonstrates that Pingora can write incremental
frames, but LLM SSE framing, usage events, disconnect cancellation, and
post-stream accounting need their own implementation and tests.

### Error Contract

Map typed provider and gateway errors to the OpenAI error envelope:

```json
{
  "error": {
    "message": "The selected model is temporarily unavailable.",
    "type": "server_error",
    "param": null,
    "code": "model_unavailable"
  }
}
```

The gateway should normalize at least these categories:

| Category | Typical HTTP status | Retry behavior |
|----------|---------------------|----------------|
| Invalid request or unsupported parameter | `400` | Never retry. |
| Authentication failure | `401` | Never retry. |
| Model or policy access denied | `403` | Never retry or reveal hidden aliases. |
| Unknown authorized model alias | `404` | Never retry. |
| Request or token limit exceeded | `413` or `422` | Never retry without changing the request. |
| Request/token/cost rate limit | `429` | Honor `Retry-After`; a different target is eligible only when policy permits. |
| Provider timeout | `504` | Retry or fallback only before visible stream output. |
| Provider unavailable or circuit open | `502` or `503` | Retry/fallback only to a capability-equivalent target. |
| Internal gateway failure | `500` | Do not expose raw provider or configuration details. |

## Proposed Architecture

```text
Client application or agent
  -> Pingora listener
  -> handler chain
       correlation -> CORS -> unified-security -> limit -> access-control -> llm
  -> OpenAI-compatible HTTP codec
  -> authenticated LLM request context
  -> alias and policy resolver
  -> token/cost/concurrency reservation
  -> cache lookup when eligible
  -> capability, residency, health, and budget target filter
  -> route selection -> retry/fallback coordinator
  -> model-provider adapter -> provider API
  -> normalized events, usage, and typed errors
  -> policy post-processing and quota reconciliation
  -> buffered JSON or SSE response
  -> metrics, trace, and privacy-aware audit event
```

### Component Boundaries

#### `apps/light-gateway`

The application should own wiring rather than provider logic:

- Register an `llm` application handler.
- Load and hold an `Arc<ConfigManager<Option<LlmGatewayRuntime>>>`.
- Register an `llm-router.yml` reloader.
- Pass the existing authenticated principal, agent delegation, correlation, and
  trace context to the LLM runtime.
- Delegate buffered and streaming response writing to the shared Pingora LLM
  integration.

The handler can be placed in a chain with existing security and traffic
handlers. A typical inference chain is:

```yaml
handlers:
  - correlation
  - metrics
  - cors
  - unified-security
  - limit
  - access-control
  - llm
```

#### `frameworks/light-pingora`

The shared framework should own Pingora-specific integration:

- Request body bounds and media-type checks.
- HTTP route matching under `/v1`.
- Extraction of authenticated and correlation context.
- Buffered response and SSE frame writing.
- Downstream disconnect detection and cancellation propagation.
- Config registration and secret masking metadata.

Provider selection, retries, cost calculations, and cache semantics should not
be embedded in `apps/light-gateway/src/main.rs`.

#### New `crates/llm-gateway`

A protocol-neutral crate should own the inference gateway runtime:

- Public alias and deployment snapshots.
- Request policy and capability validation.
- Route eligibility and selection.
- Retry, fallback, circuit breaker, and concurrency coordination.
- Exact/semantic cache interfaces.
- Usage normalization, pricing, reservation, and reconciliation hooks.
- Provider-neutral audit and metrics events.
- Translation between normalized gateway requests and `model-provider` calls.

Keeping this logic outside Pingora makes it testable without a network server
and reusable by a future sidecar or embedded inference client.

#### `crates/model-provider`

Provider clients should own provider-specific authentication, request encoding,
response decoding, event parsing, and error classification. They should not own
tenant policy or public alias routing.

Add a gateway-capable trait while retaining an adapter for the current agent
trait during migration. A representative shape is:

```rust,ignore
#[async_trait]
pub trait InferenceProvider: Send + Sync {
    fn capabilities(&self, model: &str) -> ModelCapabilities;

    async fn execute(
        &self,
        request: InferenceRequest,
        cancellation: CancellationToken,
    ) -> Result<InferenceOutput, ProviderError>;
}

pub enum InferenceOutput {
    Buffered(InferenceResponse),
    Streaming(InferenceEventStream),
}
```

The canonical contract needs:

| Area | Required types or behavior |
|------|----------------------------|
| Content | Text, image URL/data, audio/file references when supported, tool calls, tool results, refusal, and public reasoning summary blocks. |
| Requests | Operation kind, messages/items, tools, tool choice, response format, sampling, output bound, stop conditions, metadata, and streaming. |
| Events | Response start, content delta, tool-call delta, usage update, finish reason, error, and response complete. |
| Usage | Input, output, cached input, reasoning, image/audio, and provider-specific billable units where available. |
| Errors | Stable category, HTTP status, retryable flag, provider request ID, sanitized message, `Retry-After`, and whether the provider may have accepted work. |
| Capabilities | Per model and operation, including streaming, tools, parallel tools, vision, structured output, reasoning, embeddings, audio, and prompt caching. |
| Control | Deadline and cancellation propagation. |

Capabilities must be per model/deployment when provider offerings differ. A
provider-wide boolean is not enough for route safety.

### Provider Eligibility

Initial gateway work should prioritize non-interactive, server-oriented
providers. Account-login and local CLI providers remain disabled by default in
shared deployments.

Before a deployment can serve an alias, it must pass a conformance profile for
that alias's required features:

- Buffered chat request and normalized response.
- SSE stream parsing and termination.
- Usage extraction for buffered and streaming calls.
- Tool call name, ID, and incremental JSON argument preservation.
- Multimodal content conversion where advertised.
- Structured-output enforcement where advertised.
- Timeout, rate-limit, authentication, invalid-request, and server-error
  classification.
- Cancellation and body-size limits.
- Secret redaction from errors and debug logs.

An alias must fail closed when no eligible deployment can satisfy every
required capability. It must not quietly drop tools, images, JSON Schema, or a
data-residency restriction to make a fallback succeed.

## Routing And Reliability

### Selection Pipeline

For each request, filter targets in this order:

1. Resolve the public alias from one immutable config snapshot.
2. Apply caller model/operation allowlists.
3. Enforce host, tenant, agent, data-classification, and region constraints.
4. Require all request capabilities.
5. Remove disabled, unhealthy, open-circuit, or concurrency-saturated targets.
6. Remove targets that cannot fit the remaining token or cost budget.
7. Apply routing priority, weight, session stickiness, or configured strategy.

The resulting route decision and snapshot version stay attached to the request
for its entire lifetime. A config reload does not change an in-flight fallback
chain.

### Routing Strategies

Implement strategies incrementally:

- Ordered primary/fallback chain for the MVP.
- Weighted random across equivalent deployments.
- Least in-flight requests with a bounded weight bias.
- Latency-aware selection using a rolling time-to-first-token and completion
  latency window.
- Cost-aware selection subject to a minimum capability and quality tier.
- Sticky routing by authenticated principal plus bounded session ID.
- Canary and A/B allocation with an auditable stable hash.
- Region and data-boundary routing as hard eligibility rules, not soft weights.

Quality-based or semantic routers can be explored later. They must be
deterministic enough to audit, include the router's own latency and cost, and
never weaken explicit policy constraints.

### Retry, Fallback, And Circuit Rules

- Retry connection failures, timeouts, `408`, `429`, and selected `5xx`
  responses according to typed error policy.
- Do not retry authentication, authorization, invalid-request, unsupported
  parameter, context-length, or safety-policy failures.
- Honor provider `Retry-After` and apply exponential backoff with jitter.
- Bound attempts by both count and the original request deadline.
- Use a different credential or deployment only when policy allows it.
- Open a circuit after a configurable failure threshold; probe with bounded
  half-open traffic.
- Preserve required capabilities and data-boundary rules across fallback.
- Never begin a fallback after semantic stream output is visible to the client.
- Record each physical attempt separately but charge and report the complete
  logical request accurately.

The current `ReliableProvider` error-string heuristic and nested retry loops are
useful prototype behavior, but gateway reliability must use typed errors and a
single request-scoped attempt budget.

## Security And Governance

### Identity And Model Access

Reuse existing handler-chain authentication. The LLM runtime receives a trusted
identity context containing the authenticated client, user, host, issuer,
roles, agent delegation, and relevant policy snapshot.

LLM policy can then enforce:

- Allowed model aliases and operations.
- Maximum input, output, total, and reasoning tokens.
- Maximum request bytes, images/files, tool count, and JSON Schema size/depth.
- Requests per minute, tokens per minute, concurrent calls, and concurrent
  streams.
- Per-request, per-window, and lifecycle cost budgets.
- Allowed providers, regions, and data-classification boundaries.
- Whether prompts or responses may be cached or content-logged.
- Whether tools, multimodal input, structured output, or provider-native
  extensions are allowed.

The existing `limit` handler remains useful for request-count limits. Token,
cost, and model concurrency limits require usage-aware LLM accounting.

### Credential Isolation

- Provider credentials are resolved only from server-owned configuration or a
  secret reference.
- Secret values are masked in module registration, config inspection, errors,
  metrics, and logs.
- A provider base URL is validated at config load. The client cannot override
  it, preventing an inference request from becoming an SSRF primitive.
- Provider credentials should be scoped by deployment and environment rather
  than shared globally.
- Key/deployment selection is audited by opaque ID; raw secret material never
  enters the request context or audit event.
- Local CLI or account-session credentials require isolated single-user runner
  profiles and are not enabled in the shared gateway by default.

### Guardrails And Data Protection

Support ordered pre-provider and post-provider policy hooks:

- Prompt and attachment size/type validation.
- PII/tokenization or DLP policy.
- Moderation and prohibited-content policy.
- Prompt-injection and secret-exfiltration signals where configured.
- Tool schema and tool-name allowlists.
- Structured-output schema validation.
- Output redaction and data-boundary checks.

Streaming has a sharp policy boundary: a post-filter cannot retract bytes that
have already reached the client. A policy requiring whole-response inspection
must either force buffered mode, use an upstream/provider guardrail that runs
before emission, or reject streaming for that alias. Chunk-local filtering is
allowed only for policies explicitly designed and tested for bounded windows.

### Privacy-Aware Logging

Default audit events contain metadata, not content:

- Request/correlation/trace ID.
- Authenticated policy scopes.
- Public alias, internal route ID, and config snapshot version.
- Timing, status, retry/fallback count, and cancellation reason.
- Normalized usage and computed cost.
- Cache and guardrail outcomes.

Content logging is separately authorized, sampled, redacted, encrypted, and
retention-bounded. Never put prompts, completions, tool arguments, user IDs,
API keys, or model output into metric labels.

## Token And Cost Governance

Usage accounting must distinguish estimated, provider-reported, and locally
counted values.

Recommended flow:

1. Validate the alias's maximum output and estimate or count input units.
2. Atomically reserve the maximum allowed tokens and cost before provider
   dispatch.
3. Reject the request when the authoritative budget cannot reserve capacity.
4. Reconcile the reservation with trusted provider usage when the request
   finishes.
5. Retain a conservative charge or explicitly mark accounting incomplete when
   a timeout/cancellation prevents authoritative usage.
6. Store the pricing-table version and evidence source with the ledger entry.

Scopes should include host, customer/organization, team, client, user, agent,
public alias, and provider deployment as needed. Multi-replica deployments need
an authoritative shared quota store; process-local counters are not sufficient
for hard budgets.

The pricing catalog must be versioned and support:

- Input, output, cached input, and cache-creation rates.
- Reasoning, image, audio, and other provider-specific billable units.
- Tiered context pricing and provider service tiers.
- Contract-specific overrides.
- A clear `unknown` state. Unknown pricing must not silently become zero when a
  hard cost budget is configured.

## Caching

Caching is valuable but should follow routing, security, and usage correctness.

### Exact Response Cache

Add after the MVP with a cache key that includes at least:

- Authenticated tenant/policy partition.
- Public alias and alias snapshot version.
- Normalized messages/items, tools, tool choice, response format, and relevant
  sampling parameters.
- Data-boundary and guardrail policy version.

Do not cache tool-call responses, sensitive requests, or nondeterministic
requests by default. Provider prompt caching and gateway response caching are
different features and need separate metrics.

### Semantic Cache

Semantic caching is a later, explicit opt-in because similar prompts are not
necessarily interchangeable. It requires:

- Tenant- and policy-isolated vector indexes.
- A versioned embedding model and similarity threshold.
- Alias, tool, structured-output, locale, and safety-policy compatibility.
- No cross-tenant hits.
- Auditability of the matched entry and score.
- A deletion and retention model for source text and embeddings.

Cached responses still pass current authorization and post-response policy.
Usage and cost clearly distinguish cache hits from provider calls.

## MCP And WebSocket Integration

The normal agent tool loop remains:

```text
agent -> MCP router tools/list
agent -> LLM gateway chat request with selected tool schemas
LLM gateway -> model provider
model provider -> tool call
LLM gateway -> agent
agent -> MCP router tools/call
MCP router -> backend API or MCP server
agent -> LLM gateway with tool result
```

This separation preserves the MCP router as the execution, authorization, and
audit boundary. The LLM gateway must not accept a model-generated target URL or
execute a tool solely because the model emitted its name.

A later feature may let a policy-selected tool profile inject a small set of
MCP schemas into an LLM request. Even then:

- The tool set is selected by server-owned policy and the authenticated
  principal.
- Tool execution still goes through the MCP router.
- The client agent remains responsible for the tool loop unless a separately
  designed managed-agent service owns it.

The existing `websocket` handler routes browser UI traffic to agents. A future
OpenAI-compatible Realtime API has different session, audio, provider, and
billing semantics and must use a distinct handler and configuration rather than
overloading the UI router.

## Configuration Model

Use `llm-router.yml` for the data-plane projection. Provider credentials are
masked values or secret references populated through the existing runtime
configuration flow.

Illustrative configuration:

```yaml
enabled: ${llm-router.enabled:false}
pathPrefix: ${llm-router.pathPrefix:/v1}
maxRequestBodyBytes: ${llm-router.maxRequestBodyBytes:4194304}
maxResponseBodyBytes: ${llm-router.maxResponseBodyBytes:16777216}
requestTimeoutMs: ${llm-router.requestTimeoutMs:120000}
streamIdleTimeoutMs: ${llm-router.streamIdleTimeoutMs:30000}
maxConcurrentRequests: ${llm-router.maxConcurrentRequests:1024}
maxConcurrentStreams: ${llm-router.maxConcurrentStreams:512}
unsupportedParameterPolicy: ${llm-router.unsupportedParameterPolicy:reject}

providers:
  openai-primary:
    type: openai
    baseUrl: ${llm.providers.openaiPrimary.baseUrl:https://api.openai.com/v1}
    apiKey: ${llm.providers.openaiPrimary.apiKey:}
    connectTimeoutMs: 3000
    requestTimeoutMs: 90000
  anthropic-primary:
    type: anthropic
    baseUrl: ${llm.providers.anthropicPrimary.baseUrl:https://api.anthropic.com}
    apiKey: ${llm.providers.anthropicPrimary.apiKey:}
    connectTimeoutMs: 3000
    requestTimeoutMs: 90000

models:
  - name: chat-fast-v1
    operations: [chat]
    capabilities: [streaming, tools, vision]
    maxInputTokens: 128000
    maxOutputTokens: 8192
    dataClassifications: [public, internal]
    routes:
      - id: openai-fast-primary
        provider: openai-primary
        model: provider-physical-model-a
        weight: 80
        regions: [ca, us]
      - id: anthropic-fast-fallback
        provider: anthropic-primary
        model: provider-physical-model-b
        weight: 20
        fallbackOnly: true
        regions: [ca, us]

routing:
  strategy: weighted
  maxAttempts: 3
  baseBackoffMs: 100
  maxBackoffMs: 2000
  retryStatuses: [408, 429, 500, 502, 503, 504]
  circuitBreaker:
    failureThreshold: 5
    resetTimeoutMs: 30000
    halfOpenRequests: 1
  sessionHeader: X-Light-Session-Id

logging:
  content: false
  includeProviderRequestId: true
  sampleRate: 1.0
```

This is a proposed shape, not a statement that these keys are already
implemented. Before implementation, split secret-bearing provider deployment
records from public alias and routing policy if that better matches the portal
and config-server persistence model.

Configuration validation must reject:

- Duplicate alias, provider, route, or deployment IDs.
- Empty credentials for an enabled provider unless its authentication mode
  explicitly allows them.
- Invalid or unsafe provider base URLs.
- Fallback targets missing an alias's required capability or region.
- Routes to interactive providers in a shared profile.
- Impossible token or timeout bounds.
- Unknown strategy, operation, capability, or unsupported-parameter policy.
- Secret values that would be exported without a mask.

Reload builds and validates a complete candidate runtime before one atomic
swap. The previous runtime remains active when candidate validation fails.

## Feature Priorities

### MVP: Compatible And Safe Inference

- `llm` handler and reloadable `llm-router.yml` runtime.
- OpenAI-compatible `/v1/models` and `/v1/chat/completions`.
- Buffered and SSE streaming with disconnect cancellation.
- Text, tool calling, usage, and provider-supported image input.
- Public aliases and ordered primary/fallback routes.
- Server-side credentials with masking and base-URL validation.
- Existing Light authentication, access control, correlation, metrics, and
  request rate limits.
- Request, token, response, timeout, concurrency, and attempt bounds.
- Typed errors and explicit unsupported-parameter behavior.
- Provider conformance tests for the first supported deployment set.
- Metadata-only audit events and normalized usage.

### Production Hardening

- Weighted and least-in-flight routing.
- Per-target circuit breakers and active/passive health signals.
- Shared per-principal token, cost, and concurrency budgets.
- Versioned pricing and usage reconciliation.
- Model/region/data-boundary policy.
- Structured outputs and expanded multimodal conformance.
- Exact response caching.
- Pre/post guardrail hooks and strict streaming policy modes.
- Portal configuration, route inspection, usage, and budget views.
- OpenTelemetry traces and operational dashboards.
- Multi-replica chaos, failover, and stream soak testing.

### Endpoint And Intelligence Expansion

- `/v1/responses` with native normalized events.
- Embeddings, moderation, images, audio, rerank, and batch APIs when supported
  by dedicated provider operations.
- Semantic caching.
- Cost-, latency-, and quality-aware routing.
- Canary/A-B routing and policy-controlled session stickiness.
- Selected Anthropic or Google native compatibility adapters.
- Realtime inference over a dedicated WebSocket/WebRTC handler.
- Optional governed MCP tool-schema injection without in-gateway execution.

## Observability

Emit bounded-cardinality metrics for:

- Logical requests and physical attempts.
- Successes and normalized error categories.
- Active and queued requests/streams.
- Request duration, provider latency, time to first token, and stream duration.
- Input, output, cached, reasoning, and total tokens.
- Estimated and reconciled cost.
- Retries, fallback depth, circuit state changes, and route saturation.
- Cache hit/miss/bypass and guardrail outcomes.
- Downstream disconnects and upstream cancellation outcomes.

Labels can include public alias, operation, route ID, provider type, status
class, and environment when their value sets are bounded. Never label metrics
with prompt text, user-provided model strings, user IDs, session IDs, raw
provider error text, or provider request IDs.

Each trace should separate:

- Authentication and policy evaluation.
- Quota reservation.
- Cache lookup.
- Route selection.
- Each provider attempt.
- First-token wait and stream transfer.
- Guardrail processing.
- Usage and cost reconciliation.

## Testing Strategy

### Protocol Tests

- Golden request/response fixtures from current OpenAI SDKs.
- Chat message roles, content parts, tool calls, structured output, usage, and
  error envelopes.
- SSE fragmentation at arbitrary byte boundaries, multi-byte UTF-8, tool-call
  argument deltas, final usage, `[DONE]`, and midstream failure.
- SDK smoke tests with at least Python and TypeScript clients using only a base
  URL and credential change.

### Provider Contract Tests

- Provider mock servers for every supported success and error shape.
- Capability conformance by physical model/deployment.
- Authentication, rate-limit, invalid-request, context-limit, timeout, `5xx`,
  malformed JSON, oversized body, and truncated stream behavior.
- Provider request ID, `Retry-After`, usage, and cancellation extraction.
- Secret redaction from every error and log path.

### Routing And Governance Tests

- Deterministic alias resolution from one immutable snapshot.
- Capability, policy, region, and data-boundary target filtering.
- Weighted distribution and stable canary/session allocation.
- Retry/fallback deadline and attempt limits.
- Proof that no retry or fallback starts after the first semantic stream event.
- Atomic token/cost reservation under concurrency and correct reconciliation on
  success, error, timeout, and cancellation.
- Cross-tenant cache and session-stickiness isolation.

### Runtime Tests

- Candidate config rejection leaves the previous runtime active.
- Config reload does not mutate in-flight route snapshots.
- High-concurrency buffered and streaming load tests.
- Slow-client, disconnect, provider-stall, circuit-breaker, and provider-outage
  chaos tests.
- Multi-replica budget and cache tests against the selected shared stores.
- Metrics cardinality and content-leak checks.

## Rollout Plan

1. Add canonical inference types, typed errors, cancellation, and streaming to
   `model-provider`, preserving an adapter for existing `light-agent` and
   `light-workflow` callers.
2. Build provider conformance tests and enable a small server-safe provider set.
3. Add `crates/llm-gateway` with alias resolution, request validation, ordered
   fallback, usage normalization, and protocol-neutral tests.
4. Add the Pingora `llm` handler, `/v1/models`, buffered Chat Completions, and
   existing handler-chain integration.
5. Add SSE streaming and prove cancellation and no-post-output-fallback rules.
6. Add shared token/cost budgets, pricing, richer routing, guardrails, and exact
   caching.
7. Add `/v1/responses` without translating it through the less expressive Chat
   Completions representation.
8. Expand operations and provider-native compatibility only from demonstrated
   client requirements and conformance coverage.

## Acceptance Criteria For The MVP

- An OpenAI SDK can call an authorized public alias by changing only its base
  URL and credential.
- The same request can route to at least two different provider types while
  returning the same public Chat Completions shape.
- Buffered and streaming tool calls preserve IDs, names, and JSON arguments.
- Fallback works before output and is proven not to run after output begins.
- Provider credentials, base URLs, raw errors, and hidden model names do not
  leak to clients, logs, metrics, or module inspection.
- Unauthorized callers cannot enumerate or invoke hidden aliases.
- Request, token, output, timeout, concurrency, and attempt bounds fail closed.
- Usage is recorded for success, failure, timeout, and cancellation with an
  explicit completeness/evidence state.
- A bad `llm-router.yml` reload leaves the last valid runtime active.
- Existing MCP and WebSocket routes continue to work through their current
  handler chains.

## References

- [Bifrost repository](https://github.com/maximhq/bifrost)
- [Bifrost overview](https://docs.getbifrost.ai/overview)
- [Bifrost drop-in replacement](https://docs.getbifrost.ai/features/drop-in-replacement)
- [Bifrost retries and fallbacks](https://docs.getbifrost.ai/features/retries-and-fallbacks)
- [Bifrost governance routing](https://docs.getbifrost.ai/features/governance/routing)
- [LiteLLM repository](https://github.com/BerriAI/litellm)
- [LiteLLM documentation](https://docs.litellm.ai/)
- [OpenAI API reference](https://platform.openai.com/docs/api-reference)
- [OpenAI Responses streaming events](https://platform.openai.com/docs/api-reference/responses-streaming)
