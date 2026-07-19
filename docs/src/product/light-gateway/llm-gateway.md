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
- Store the host-scoped model catalog, deployments, public aliases, routing
  policy, capability snapshots, and pricing metadata in the Light Portal
  control plane. Agent definitions reference a governed alias or model policy;
  they do not own provider credentials or select a physical provider model.
- Separate control-plane, inference-record, and reversible-PII storage. Portal
  PostgreSQL remains authoritative for configuration and canonical agent-domain
  events. A dedicated local or regional audit store owns gateway inference
  records, while a separately credentialed PII vault is used only when token
  mappings must survive the request.
- Keep request-scoped PII mappings in memory by default. Use a local bounded
  WAL/spool for audit delivery, not as the authoritative audit corpus or a
  replica-local long-lived PII vault. Distinguish `bounded-async` admission
  from `local-durable` pre-dispatch commit; never claim that queue capacity is
  crash durability.
- Represent the client format, logical operation, and selected upstream format
  separately. Preserve unknown fields in a bounded compatibility envelope for
  same-format forwarding, and upgrade to fully typed canonical content only
  when policy mutation or cross-provider conversion requires it.
- Pre-bind provider dispatch, resolved alias policy, eligible priority groups,
  pricing references, and content-access requirements into an immutable
  runtime snapshot. Static enum and preconstructed dynamic dispatch are both
  acceptable; the benchmark decides. A request must not repeatedly lock
  configuration stores, merge policy layers, construct a provider client, or
  look up provider implementations by string.
- Publish one small atomic root containing structurally shared routing,
  provider, policy, and pricing sub-snapshots. A pricing-only or single-alias
  update reuses unchanged `Arc` graphs, while one root load still gives each
  request a generation-consistent view.
- Treat every upstream credential as an authorized quota and billing
  principal. Credentials in one deployment set are lifecycle versions within
  the same quota group; separately approved accounts/capacity are separate
  deployments. The gateway must not rotate keys to evade a provider's RPM/TPM,
  account, contract, or abuse limits.
- Make performance a release contract, not an implementation claim. The
  gateway must meet an absolute latency and capacity SLO and must also equal or
  outperform a pinned Bifrost build under the same open-loop workload,
  hardware limits, protocol, payloads, provider mock, and enabled features.

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

Three open source gateways provide useful feature signals:

- [Bifrost](https://github.com/maximhq/bifrost) emphasizes an OpenAI-compatible
  API, provider-native compatibility adapters, retry and fallback, weighted
  routing, virtual-key governance, hierarchical budgets, semantic caching,
  plugins, observability, and MCP integration.
- [LiteLLM](https://github.com/BerriAI/litellm) exposes OpenAI-format and native
  endpoints across many providers and adds proxy authentication, virtual keys,
  spend tracking, rate limits, routing, fallback, caching, guardrails, and
  logging.
- [agentgateway](https://github.com/agentgateway/agentgateway) is a Rust
  multi-protocol gateway with purpose-built local and xDS configuration, an LLM
  model router, OpenAI and provider-native formats, typed provider conversion,
  virtual models, health-aware priority failover, guardrails, token/cost
  telemetry, and an atomically replaceable pricing catalog. The implementation
  review in this document is based on commit
  [`857281d`](https://github.com/agentgateway/agentgateway/tree/857281d108d5444b92ed66f5e4733225a4990426).

The Light design should adopt the durable product capabilities without copying
any reference project's control plane. Light already has a portal, config server,
controller, security handlers, access control, and an MCP router.

## Goals

- Give applications and agents one stable base URL and one common API across
  supported model providers.
- Let existing OpenAI SDK users migrate by changing the base URL and client
  credential rather than rewriting request and response handling.
- Support buffered and streaming chat, tool calling, structured output, and
  supported multimodal input without losing provider semantics silently.
- Route public model aliases to one or more physical provider deployments.
- Let each organization/host register only the models and deployments it is
  authorized to use, and manage their routing metadata through GenAI Admin.
- Provide retry, fallback, load balancing, circuit breaking, health-aware
  routing, and cancellation with well-defined streaming behavior.
- Enforce model access, data-boundary constraints, token limits, cost budgets,
  concurrency limits, and request rate limits per authenticated identity.
- Record normalized usage, cost, latency, time to first token, route decisions,
  retry/fallback activity, and policy outcomes.
- Deliver audit records without adding synchronous Portal-database work to the
  normal inference path, and support governed content capture for later audit,
  evaluation, and curated dataset export.
- Tokenize policy-selected PII before cloud-provider dispatch and recover only
  exact authorized placeholders before returning the model response.
- Protect provider credentials and prevent clients from choosing arbitrary
  upstream URLs or passing provider secrets through the gateway.
- Reload provider, alias, route, and policy snapshots atomically without
  interrupting in-flight requests.
- Make provider conformance measurable so an alias is offered only when every
  eligible target can satisfy its declared capabilities.
- Sustain Bifrost-class request rates without entering a queueing collapse:
  keep the hot path typed and allocation-conscious, reuse upstream clients,
  shed excess load promptly, and verify comparative throughput and tail
  latency before release.

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
- Do not use the Portal database, a gateway replica's embedded database, or the
  inference content store as the production reversible-PII security boundary.
- Do not treat operational audit content as an automatically approved training
  dataset.
- Do not promise identical model output after fallback. Fallback preserves the
  API and required capabilities, not model behavior.
- Do not advertise a fixed multiple such as `40x` or `50x`. Those ratios depend
  on the benchmark definition and can be dominated by overload queueing. Report
  gateway-added latency, sustainable throughput, success rate, and resource
  use from a reproducible benchmark instead.

## Reference Feature Comparison

The comparison is a requirements input, not a compatibility promise.

| Capability | Bifrost signal | LiteLLM signal | agentgateway signal | Light direction |
|------------|----------------|----------------|---------------------|-----------------|
| Common inference API | OpenAI-compatible API plus provider SDK adapters. | OpenAI input/output format plus native endpoints. | OpenAI Completions/Responses plus Anthropic Messages, embeddings, rerank, realtime, token count, detect, and opaque routes. | OpenAI-compatible API first; preserve source-format identity so selected native adapters can be added without flattening through Chat Completions. |
| Provider abstraction | Many hosted and local providers. | Broad provider and endpoint coverage. | Rust enum dispatch with typed request/response conversions and provider-format selection. | Reuse `model-provider`, gated by per-operation conformance tests; pre-bind provider execution in the runtime snapshot and let allocation benchmarks choose enum or dynamic dispatch. |
| Routing | Provider/model/key routing and weighted strategies. | Deployment router and load-balancing strategies. | Public/internal concrete models plus weighted, conditional, and health-aware priority-failover virtual models. | Public alias to eligible deployment targets with weighted, priority-, health-, policy-, and capability-aware selection. |
| Reliability | Retries, key rotation, and sequential fallbacks. | Retries, cooldowns, and cross-deployment fallback. | Generic HTTP retry integrates with endpoint outlier eviction so the next attempt can move to the next priority group. | One typed attempt coordinator, `Retry-After`, health/outlier state, and no fallback after visible stream output. |
| Tenant credentials | Virtual keys. | Virtual keys and proxy keys. | General gateway authentication and authorization policies apply to LLM routes/models. | Reuse Light authentication; map the authenticated client, user, agent, and host to an LLM policy. |
| Cost governance | Hierarchical budgets and rate limits. | Spend tracking and budgets by several scopes. | Token-aware rate limits and an `ArcSwap` pricing catalog with detailed usage classes and source overlays. | Atomic token/cost reservation and usage reconciliation by configured Light policy scopes; publish pricing as an independent versioned projection. |
| Caching | Exact/provider and semantic caching. | Configurable response caches. | Provider prompt-caching policy and cache-token accounting. | Exact cache later; semantic cache is opt-in and tenant/policy isolated. |
| Guardrails | Plugin-based request and response controls. | Per-project guardrails and callbacks. | Local regex/PII masking, external safety services, and bounded-window SSE/realtime response blocking. | Ordered local/remote hooks, reversible PII profiles, and explicit buffered versus bounded-window streaming semantics. |
| Observability | Metrics, tracing, and request logging. | Logging callbacks, usage, cost, and latency. | CEL-selectable LLM attributes, normalized usage/cost, and streaming completion accounting. | Existing correlation/metrics plus bounded-cardinality events and a dedicated durable audit pipeline. |
| Performance architecture | Compiled Go, `fasthttp`, typed provider codecs, object pools, and per-provider workers. | FastAPI/ASGI with a generic Python router, SDK dispatch, and callback pipeline. | Compiled Rust, typed/minimally parsed codecs, reusable clients, bounded bodies, endpoint sets, and atomic pricing snapshots; some configuration reads and policy merging remain request-time work. | Rust/Pingora, compatibility fast path, pre-bound provider dispatch, one structurally shared request snapshot, bounded admission, and benchmark-enforced parity or better. |
| MCP | MCP gateway and tool filtering. | MCP support and model-tool integration. | MCP, A2A, HTTP, and LLM backends share the gateway policy/runtime. | Keep the existing MCP router authoritative for tool discovery and execution while sharing identity, policy, and telemetry primitives. |
| Administration | Built-in configuration and monitoring UI. | Admin UI and APIs. | Human-friendly watched local config or granular purpose-built xDS resources mapped to a shared IR. | Use Light Portal, config server, and controller; publish granular resources but compile a complete request-ready runtime snapshot. |

### agentgateway Architecture Review

The reviewed agentgateway path is not merely "Rust instead of Python." Its
architecture makes several explicit choices that reduce compatibility work and
keep most LLM processing inside compiled code:

```text
local YAML/JSON or purpose-built xDS resources
  -> shared gateway IR and targeted policies
  -> HTTP route -> LLM model router
  -> concrete model or weighted/conditional/failover virtual model
  -> provider endpoint set and merged backend policy
  -> client-format parser -> provider-format renderer
  -> reusable upstream transport
  -> provider stream parser -> client-format stream renderer
  -> optional bounded-window guard -> client
```

The following decisions are worth adopting or deliberately refining:

| agentgateway choice | Why it is useful | Light decision |
|---------------------|------------------|----------------|
| Separate endpoint `RouteType`, client `InputFormat`, and provider `ChatFormat`. | A Messages request may go to a Completions upstream while the gateway still knows which response contract it owes the client. | Define `ClientFormat`, `Operation`, and `ProviderFormat` separately from day one. Never infer the response contract from the selected provider route. |
| Parse operated fields and preserve unknown fields in a flattened `rest`; upgrade to fully typed forms only for conversion. | Same-format compatibility survives provider/API additions without requiring an immediate full schema update. | Use a bounded compatibility envelope for same-format routes. Validate operated fields strictly; allow unknown fields only under an alias/provider allowlist and never forward an unknown extension across formats blindly. |
| Public/internal concrete models and virtual models with weighted, conditional, or priority-failover routing. | Client aliases stay stable while internal targets and rollout policy change. Internal targets need not be directly invokable. | Keep public aliases separate from internal deployments. Add priority groups and metadata-conditional routing after the ordered MVP, with expressions compiled at publication and evaluated only over an allowlisted, sanitized context. |
| Provider selection uses endpoint sets with health, latency, pending-work scoring, priority buckets, and outlier eviction. | A retry can reselect after a bad target is ejected instead of repeatedly hitting the same endpoint. | Treat retry and failover as one attempt coordinator. Feed typed outcomes into per-deployment health and reselect from the same immutable eligible plan, preserving capability and residency constraints. |
| Built-in providers use exhaustive enum dispatch and typed conversion code. | Provider choice is resolved in compiled Rust without constructing a dynamic SDK/client per request. | Preserve the pre-bound compiled path but benchmark sealed-enum and preconstructed trait-object implementations. Never do string-to-provider registry lookup or client construction on each attempt. |
| Streaming is translated to the client-visible SSE format before response guards inspect text. Held semantic frames are released in bounded windows with overlap. | Binary or provider-native streams are not mistaken for OpenAI SSE, and patterns spanning chunks can be detected before held frames are released. | Apply provider decoding first, then exact PII-token recovery and post-policy in a documented order over semantic events. Buffer complete frames plus a bounded overlap; a whole-response rule forces buffered mode. |
| Prompt/completion attributes are materialized only when a CEL expression asks for them, and the raw LLM request exists only during the LLM-policy phase. | Large, sensitive content does not become a universal request-context cost or remain available to later logging accidentally. | Make content access lazy, phase-scoped, and policy-authorized. Remove raw content from the general handler/log context after the content-policy phase; later stages receive normalized metadata or explicit encrypted references. |
| Pricing sources merge into a validated `ArcSwap` snapshot and retain the last valid catalog on reload failure. | Pricing reads are wait-free and a broken file does not turn known prices into zero. | Publish pricing as an independently versioned immutable projection, capture its version per attempt, support explicit source precedence, and keep `unknown` pricing fail-closed for hard budgets. External reference catalogs never authorize a host or activate a deployment. |
| Local/xDS resources closely mirror user resources, while policies remain separate and merge at runtime. | Small control-plane changes avoid large route-list fan-out and keep control-plane translation mechanical. | Preserve granular Portal/config events and delta publication, but compile affected alias/route policy combinations before activation. The LLM request path loads one request-ready snapshot and performs no shared-store policy merge. |

There are also boundaries Light should not copy:

- The reviewed gateway store uses shared `RwLock`-protected bind/discovery
  stores, and the HTTP/LLM path reacquires bind reads and clones/merges some
  policy layers during request processing. Light should spend the additional
  reload-time work to publish pre-resolved LLM plans behind `ArcSwap`.
- agentgateway may inject `stream_options.include_usage=true` when the client
  omitted it, which adds a client-visible final SSE event. Light may request
  upstream usage internally, but it must remember the original client contract
  and suppress an injected usage frame unless the client requested it.
- Its bounded-window streaming guardrail explicitly cannot provide full-stream
  accuracy, cannot retract earlier windows, and does not support streaming
  masking. Light policy publication must reject an incompatible
  streaming/policy combination or force buffering; reversible exact-token
  recovery is a separate bounded streaming transform, not ordinary masking.
- Its local PII recognizers mask or reject content; they do not provide the
  separately scoped reversible-token vault required by this design. Its
  telemetry path also does not replace Light's logical-request/physical-attempt
  durable audit ledger and governed dataset export.
- A provider response parsing helper in the reviewed source logs up to the
  first 1,024 response bytes on a parse failure. Light must never put raw
  provider error/response bodies into ordinary logs. Record only bounded error
  classification, byte length, content type, and a keyed digest unless an
  explicitly authorized encrypted-content policy captures the body.

These differences are opportunities to be faster as well as safer than the
reference: agentgateway validates the value of typed Rust codecs, static
provider dispatch, endpoint priority groups, and atomic catalog replacement;
Light can combine those ideas with a stricter one-published-root request path
and no request-time configuration merging.

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

Internally, keep three dimensions distinct:

- `ClientFormat` is the request and response contract owed to the caller, for
  example OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages.
- `Operation` is the semantic action, for example chat, responses, embeddings,
  rerank, token count, or realtime.
- `ProviderFormat` is the selected upstream wire contract, for example OpenAI
  Completions, Anthropic Messages, Bedrock Converse, or a provider-native
  embedding route.

A provider selection can change `ProviderFormat`; it never changes
`ClientFormat`. This prevents a fallback or cross-provider conversion from
accidentally returning the upstream provider's shape to the client.

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

A later provider-native compatibility adapter may use one of three explicit
processing modes:

- `normalized`: strict typed validation, full policy support, and conversion to
  any conforming provider format.
- `detect`: same-format forwarding with shallow LLM metadata and usage
  extraction. It is eligible only for policies whose required controls can be
  enforced without full content normalization.
- `opaque`: bounded HTTP/WebSocket forwarding with no LLM interpretation. It
  cannot satisfy token, content-guardrail, reversible-PII, or normalized-audit
  requirements. If enabled for a separately governed compatibility route, it
  still enforces identity, destination allowlists, request/response bytes,
  request rate, concurrency, duration, egress, and a pessimistic fixed
  per-request cost reservation or externally reconciled account-spend ceiling.
  A policy requiring authoritative token or exact per-call cost accounting
  cannot select it.

The public OpenAI-compatible endpoints use `normalized`. `detect` and `opaque`
are explicitly configured compatibility tools, never automatic fallbacks when
normalization fails.

Opaque traffic is therefore not free or unlimited; its governance unit is a
bounded request rather than a token. The audit record marks token usage and
per-call realized cost as `unknown`, records the reserved fixed envelope and
byte counts, and reconciles provider-account spend asynchronously when billing
data is available. If no conservative envelope or authoritative account cap is
configured, publication rejects the route.

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
6. The gateway preserves the client's `stream_options.include_usage` choice.
   A provider adapter may request usage upstream for accounting, but an
   internally injected usage event is removed from the client stream when the
   public contract did not request it.

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
  -> client format + operation + bounded compatibility envelope
  -> authenticated LLM request context
  -> alias and policy resolver
  -> token/cost/concurrency reservation
  -> cache lookup when eligible
  -> capability, residency, health, and budget target filter
  -> route selection -> retry/fallback coordinator
  -> optional request-scoped PII tokenization
  -> statically selected model-provider adapter -> provider-format API
  -> provider decode -> client-format semantic events, usage, and typed errors
  -> exact-token PII recovery, policy post-processing, and quota reconciliation
  -> buffered JSON or SSE response
  -> metrics and trace
  -> bounded audit queue -> local spool when needed -> dedicated audit store
```

The control plane publishes immutable, versioned runtime snapshots to the
gateway. Portal, config server, secret manager, audit database, and PII vault
lookups are not part of the normal alias-resolution or routing path.

The `llm` application handler terminates the HTTP request inside the gateway;
it is not an ordinary upstream proxy. Consequently, body-dependent security
and transformation stages must execute inside the application-handler flow
before provider dispatch. Merely listing `access-control`, `tokenize`, or
another body handler earlier in `handler.yml` does not prove that Pingora's
later proxy body filters will run.

## Performance Architecture And Release Contract

Performance is part of the public reliability contract. A fast provider does
not compensate for a gateway that consumes excessive CPU, accumulates an
internal backlog, or delays stream chunks. Conversely, a microbenchmark that
excludes JSON, middleware, or response processing does not represent what a
client experiences.

### Interpreting The Bifrost Reference

The published Bifrost comparison contains two distinct results:

- At the advertised 500-RPS load, Bifrost reported about `9.5x` LiteLLM's
  completed throughput, while the reported P50 and P99 latency ratios grew to
  approximately `48x` and `54x` after LiteLLM saturated and requests queued.
- In a separate test with a 60-ms mock provider, the end-to-end medians were
  60.99 ms and 100 ms, a `1.64x` difference. The `40x` claim comes from
  subtracting the assumed 60-ms mock time and comparing 0.99 ms with 40 ms.
- Bifrost's 5,000-RPS internal-overhead figures exclude at least the upstream
  call and some codec work. They are useful implementation signals, but they
  are not directly comparable to an end-to-end proxy latency percentile.

The Light target is therefore not "be 50 times faster than LiteLLM." The target
is to remain below the saturation knee, equal or exceed Bifrost's sustainable
throughput, and equal or improve its gateway-added P50, P95, and P99 latency in
a controlled comparison. Both the absolute and comparative gates below must
pass.

### Release Performance Gates

The first implementation establishes a checked-in benchmark manifest with the
exact Light commit, Bifrost image digest or commit, load generator version,
kernel and CPU architecture, instance limits, configuration, payload corpus,
and mock-provider build. A result without those inputs is diagnostic only and
cannot satisfy a release gate.

| Gate | Required result |
|------|-----------------|
| Comparative non-inferiority | On identical hardware and feature-equivalent profiles, Light sustainable throughput must be at least Bifrost's, and Light gateway-added P50, P95, and P99 must be no higher. Compare five or more steady-state runs and require the 95% confidence interval to remain inside a 5% non-inferiority margin. The engineering target is at least 10% better throughput or P99, not merely a statistical tie. |
| Rust architecture reference | Run the same compatible subset against a pinned agentgateway commit. Report results even though Bifrost remains the MVP release comparator. Any regression against agentgateway in routing-only, same-format, or streaming profiles requires an explained architectural cause and an accepted optimization plan. |
| 500-RPS small-payload baseline | On 2 vCPU and 4 GiB RAM with keep-alive and a 60-ms mock provider, admit and complete the full 500-RPS offered load with 100% success, add no more than 1 ms at P50 and 5 ms at P99, and show no growing internal queue during the steady-state window. |
| 5,000-RPS high-throughput profile | On 4 vCPU and 16 GiB RAM with a local mock provider and small buffered responses, admit and complete the full 5,000-RPS offered load with 100% success, keep P99 admission wait below 1 ms, and meet the comparative Bifrost latency and throughput gate without unbounded memory growth. |
| Production handler profile | Repeat the comparison with correlation, cached authentication, authorization, request limits, metrics, routing, usage accounting, and metadata-only `bounded-async` audit enabled. No feature may be disabled only for Light if its equivalent remains enabled for Bifrost. |
| Durable-audit profile | Run `local-durable` metadata audit on declared persistent storage and report commit-batch size, `fdatasync` duration, commit-wait P50/P95/P99, throughput, incomplete recovery, and overload behavior. It must meet its configured commit timeout with no unaudited dispatch; do not average it into or use it to weaken the normal 500/5,000-RPS gates. |
| Streaming profile | At matched concurrent streams and chunk cadence, Light time-to-first-byte overhead and P99 per-chunk processing delay must be no worse than Bifrost. Slow consumers must remain bounded and cancellation must release permits and upstream work promptly. |
| Overload profile | Increase fixed offered load beyond capacity. Admitted-request latency must remain bounded; excess requests must receive a prompt `429` or `503` instead of waiting in an unbounded queue. The report must show the capacity knee, rejection rate, queue wait, memory, and recovery after load falls. |
| Resource profile | At matched throughput, Light peak RSS and CPU per completed request must be no worse than Bifrost. Any optional pool or cache must have a configured bound and a measured benefit. |

The numeric absolute targets are initial release floors. After the first stable
baseline they may be tightened, but a configuration or feature addition cannot
silently weaken them. If a stricter policy profile performs synchronous remote
work by design, publish it as a separate profile with its own SLO rather than
averaging it into the normal data-plane result.

### Benchmark Method

- Use a fixed-rate, open-loop generator for capacity and overload tests. A
  fixed number of virtual users is a separate closed-loop test and must not be
  labelled as RPS.
- Measure the mock provider directly in the same run. Report complete
  end-to-end latency and gateway-added latency, but never use subtraction as
  the only release metric.
- Warm DNS, TLS, connection pools, provider codecs, and lazy metrics before the
  measurement window. Report cold-start behavior separately.
- Run small, 10-KiB, and tool/schema-heavy request profiles; buffered and SSE
  response profiles; HTTP/1.1 and HTTP/2 where supported; and TLS on and off.
- Use the same upstream protocol, keep-alive policy, connection count, mock
  latency distribution, response payload, timeout, retry count, and logging
  policy for both gateways.
- Record histograms rather than averages: P50, P95, P99, P99.9 and maximum for
  end-to-end latency, gateway-added latency, admission wait, route selection,
  request/response codecs, time to first token, and stream-chunk processing.
- Record offered, admitted, completed, rejected, failed, retried, and cancelled
  requests separately. A rejected request is not a successful completion, and
  a request completed after the measurement window cannot inflate throughput.
- Capture CPU, RSS, allocation rate, task count, open connections, queue depth,
  and upstream pool reuse throughout the run. Preserve raw results as CI
  artifacts so regressions can be investigated.

### Hot-Path Rules

The normal request path follows these rules:

1. Read, decompress, and bound the HTTP body once. Parse the operated routing
   and policy fields once into a typed compatibility envelope. For same-format
   forwarding, preserve allowlisted unknown fields without a second generic
   JSON parse/serialize cycle. Upgrade to full canonical typed content only
   when an enabled policy mutates content or the selected provider format
   differs. A failed typed parse never falls back to opaque forwarding.
2. Capture one immutable `Arc<LlmPublishedSnapshot>` root at request admission.
   The root contains versioned `Arc` subgraphs for routing, provider bindings,
   effective policies, and pricing. Alias maps, capability masks, policy
   decisions, eligible route lists, weights, pricing references, and compiled
   hook lists are prepared during reload. Request processing does not scan
   configuration files or reacquire a config lock at each stage.
3. Make the root read wait-free, for example with `ArcSwap`. A pricing-only or
   single-alias publication creates a small new root that reuses every
   unchanged subgraph; it does not rebuild one monolithic allocation. The
   current runtime `ConfigManager` uses an `RwLock<Arc<T>>`; it is a functional
   reload baseline, but the LLM data plane must not multiply read-lock
   acquisitions across routing, policy, provider, and streaming stages. The
   existing `config-loader` `ArcSwap` implementation is a reusable pattern.
4. Reuse one configured HTTP client and connection pool per provider
   deployment. Never create a `reqwest::Client`, TLS configuration, DNS
   resolver, or credential object per request. Maintain separate bounded
   clients only when streaming timeouts or transport policy actually differ.
5. Keep provider translation in Rust using typed request, response, error, and
   stream-event codecs. Do not route a request through a scripting runtime,
   generic SDK dispatcher, thread-pool bounce, or serialize/deserialize bridge.
6. Compile optional hooks into the snapshot. A disabled hook creates no task,
   future, dynamic lookup, log object, or channel message on the hot path.
7. Keep local routing and policy decisions in memory. Database, Redis, portal,
   config-server, secret-manager, and pricing refreshes run outside the normal
   request path. A dependency needed for fail-closed policy is warmed and
   projected into the snapshot before it becomes active.
8. Update counters and histograms in process. Enqueue audit and usage records to
   bounded asynchronous sinks. `bounded-async` reserves envelope capacity but
   does not claim crash durability. `local-durable` waits only on the bounded
   single-writer WAL commit watermark defined by the audit contract; when
   capacity or durability is unavailable, fail before dispatch instead of
   performing an unbounded synchronous database write.
9. Preserve byte buffers with `bytes::Bytes` or equivalent ownership where the
   Pingora and provider boundaries allow it. Allocate owned strings only for
   values that must outlive the input buffer or be transformed.
10. Add pooling only after allocation profiles identify a benefit. Bifrost's
    large prewarmed pools trade memory for speed; Light should prefer bounded
    buffers, connection reuse, and fewer allocations over a large speculative
    object pool.
11. Resolve provider dispatch while building the snapshot. The hot path invokes
    one pre-bound executor and does not hash a provider name, build a client, or
    construct a chain of provider wrappers. Benchmark a sealed enum/static
    executor against a preconstructed `Arc<dyn InferenceProvider>` under the
    5,000-RPS and allocation profiles. Use dynamic dispatch when its confidence
    interval remains within the release margin; use static dispatch only when
    it provides a material measured benefit worth the maintenance cost.
12. Compile policy precedence and content requirements at publication. Each
    alias plan contains its effective policy, compiled conditional expressions,
    priority groups, and whether prompt/completion materialization is needed.
    Request processing never reacquires a control-plane `RwLock` or clones and
    merges policy maps.

Publication limits build CPU, peak temporary memory, and retired generations.
Unchanged nodes are structurally shared, dynamic health/in-flight counters stay
outside immutable configuration snapshots, and only affected alias plans are
recompiled. In-flight requests retain old `Arc` generations; cleanup drops
large retired graphs incrementally on a non-request worker so a frequent update
cannot cause a latency spike. A retirement manager keeps the final non-request
reference until request references drain, ensuring an inference task is not the
thread that recursively frees a large graph. If retained generations exceed a
bound, publication is coalesced or backpressured rather than growing memory
without limit.

The production benchmark covers the complete Light handler chain, not only
`crates/llm-gateway`. If repeated shared-runtime lock acquisitions or body
copies outside the LLM crate prevent the target, migrate those reads to a
request-scoped immutable handler bundle or a wait-free snapshot. They cannot be
excluded from the reported gateway overhead.

### Admission, Concurrency, And Backpressure

Use bounded admission before expensive parsing, token counting, guardrails, or
provider dispatch:

- Maintain global, per-principal, per-alias, and per-target in-flight permits.
  Reuse the fail-fast semaphore pattern already used by MCP resource admission.
- The default provider queue length is zero: dispatch immediately when a permit
  is available or return a sanitized `429`/`503`. An explicitly enabled queue
  is bounded by both depth and wait deadline and exposes its wait time in
  metrics.
- Reserve separate capacity for buffered requests and long-lived streams so a
  stream flood cannot starve short inference calls.
- Apply per-principal and per-source limits before a stream acquires global
  capacity. Bound request-header/body read time, stream setup time, absolute
  stream lifetime, downstream write-progress time, and idle time separately;
  a heartbeat or one-byte read must not renew every deadline indefinitely.
- Select a target only after a permit can be acquired, or retry selection from
  the remaining eligible targets. Do not select a saturated target and then
  build a deep hidden backlog behind it.
- Token counting, JSON Schema compilation, DLP, and other CPU-heavy policies
  use bounded dedicated executors and admission limits. They must not block
  Pingora request processing or Tokio worker threads.
- Release permits on every success, error, timeout, downstream disconnect,
  failed stream setup, and panic boundary. Tests must prove permit recovery.

For hard multi-replica budgets, acquire bounded token/cost leases from the
authoritative store and reserve from local atomics. Refresh leases
asynchronously before exhaustion. A policy that requires a central transaction
for every request is a separately named strict-accounting profile and cannot be
the default high-throughput path.

### Streaming Data Path

- Parse each upstream SSE event once and translate directly into one canonical
  event and one client frame. Do not accumulate the full completion unless a
  configured policy explicitly requires buffering.
- Decode provider-native transport before applying client-visible response
  policy. A Bedrock event stream, Anthropic SSE event, or OpenAI chunk must
  first become a semantic client-format event; guardrails must not scrape
  arbitrary raw byte chunks.
- Use a small bounded channel or direct backpressured writer between provider
  decoding and Pingora. A slow client must pause bounded upstream reads and
  eventually cancel; it must not create an unbounded per-stream queue.
- Enforce a downstream write-progress deadline and a minimum sustained drain
  rate after a bounded grace period. When either is violated, close the client
  stream, cancel upstream work, finalize partial usage/audit evidence, and
  release all permits. The maximum stream lifetime is an absolute deadline;
  SSE comments, TCP trickle reads, and provider heartbeats do not extend it.
- Avoid per-chunk task creation, tracing spans, JSON maps, and log writes.
  Maintain request-scoped counters and emit one summarized completion event.
- Detect downstream closure promptly, cancel the upstream request, close the
  channel, release permits, and reconcile the best available usage evidence.
- Keep stream event buffers bounded independently from maximum response bytes
  and test fragmented UTF-8, large tool arguments, rapid tiny chunks, provider
  stalls, and slow downstream consumers.
- A bounded-window response guard holds complete semantic frames until its
  threshold or maximum held-byte limit, evaluates the pending text with a
  bounded overlap from the previous window, and then releases or blocks the
  held frames. Publication records the accepted false-negative/context tradeoff
  explicitly. Policies needing full-response context force buffered mode.
- A text guard may prefer a sentence or punctuation boundary when one occurs
  inside its configured byte/time window, improving local DLP context without
  waiting for arbitrary raw chunks. This is only a flush heuristic: hard
  maximum bytes, maximum hold time, and overlap still apply because generated
  text and tool-call JSON may contain no sentence boundary. Exact PII
  placeholder recovery continues to use its bounded token-prefix state machine,
  not sentence segmentation.

### Component Boundaries

#### `apps/light-gateway`

The application should own wiring rather than provider logic:

- Register an `llm` application handler.
- Load and hold an `Arc<LlmRuntimeStore>` that exposes one wait-free
  `Arc<LlmPublishedSnapshot>` root load per request. It may reuse the existing
  `config-loader` `ArcSwap` manager or a runtime-wide equivalent.
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

##### MVP Application-Body Execution Contract

The current handler registry constructs descriptors whose executable contract
is only `PingoraHandler::id()`. `GatewayProxy::request_filter` resolves the
ordered IDs and dispatches behavior with a match, while body-dependent traffic
and access-control work normally completes later in Pingora's
`request_body_filter`. MCP is an application-handler precedent: it reads and
answers its request directly from `request_filter`. Copying that pattern
without an explicit LLM body-policy stage would let the application response
bypass the later generic body filters.

For the MVP, do not make a repository-wide executable-handler-trait refactor a
prerequisite. Register `llm` in the existing handler registry and add a narrow
branch in `GatewayProxy`, but immediately delegate to a shared
`LlmHttpIntegration` owned by `light-pingora`. That integration executes this
contract exactly once:

1. Pre-body handlers before `llm` run in configured order. Correlation,
   authentication, CORS, request-rate limits, and header policy populate the
   request context or terminate the request.
2. `llm` verifies the method, route, media type, content encoding, declared
   length, and body-read deadline, then collects at most the configured body
   limit into one `Bytes`-backed capture. No downstream handler rereads the
   socket or independently buffers the JSON body.
3. If `access-control` appeared earlier in the resolved chain, the integration
   invokes the existing endpoint authorization once with the authenticated
   principal, trusted headers, endpoint, correlation ID, and parsed request
   data. It does not rely on `request_body_filter` to perform that check later.
4. The OpenAI codec validates the request and resolves the public alias. The
   protocol-neutral LLM runtime then applies host registration, alias/model
   policy, capability, data-boundary, and admission checks. Both authorization
   layers must allow the request before an audit marker or provider attempt is
   created.
5. Buffered response policy runs before the response header/body is written.
   SSE aliases may use only streaming-safe LLM policy; a generic whole-response
   filter forces buffered mode or makes the alias invalid at publication.
6. The shared writer emits buffered JSON or SSE, propagates disconnect
   cancellation, finalizes audit/usage, and releases every permit.

The LLM request context captures the active access-control runtime and the
published LLM root once. A reload cannot change either decision halfway through
the request. Generic `tokenize`/`detokenize` handlers are rejected in an LLM
chain until they are explicitly adapted to this application-body contract;
LLM-aware PII policy belongs in `crates/llm-gateway` and its normalized content
pipeline. This prevents a configured handler from appearing active while
silently doing nothing.

After the vertical slice is benchmarked, the same integration interface can
be generalized for MCP and other application handlers. That refactor is
accepted only if it preserves handler ordering and improves maintainability or
measured copies/locks; it is not required to obtain the first LLM benchmark.

#### `frameworks/light-pingora`

The shared framework should own Pingora-specific integration:

- Request body bounds and media-type checks.
- One-pass bounded body collection with reusable byte buffers; downstream
  handlers receive the same captured body rather than independently reading or
  copying it.
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
- Precomputed, wait-free request snapshots and bounded admission permits.
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

The current server-oriented providers already construct and retain a
`reqwest::Client`, which supplies connection pooling across calls. The gateway
runtime must preserve that lifecycle. Runtime construction creates provider
clients once per validated deployment snapshot; request execution borrows the
client and must not rebuild transport state.

Add a gateway-capable operation contract while retaining an adapter for the
current agent trait during migration. Each published `ProviderBinding`
contains a preconstructed client, per-model capabilities, supported provider
formats, typed request/response/stream codecs, and one pre-bound executor.

The contract does not mandate enum or trait-object dispatch before measurement:

- A sealed built-in enum provides exhaustive compile-time dispatch and avoids
  a boxed async future, but centralizes provider variants and can increase
  maintenance coupling.
- A preconstructed `Arc<dyn InferenceProvider>` keeps provider crates and
  optional extensions independent, but may add a virtual call and boxed future.
- Both implementations must expose the same provider conformance suite and be
  benchmarked with real allocation profiles. Only a material, repeatable
  release-profile difference justifies making static dispatch mandatory.

Whichever representation wins, it is bound during publication. The request
path never resolves a provider by string, constructs a trait object, creates a
transport client, or stacks retry/routing wrapper providers dynamically.

The request codec owns the bounded compatibility envelope and canonical typed
content. The provider renderer receives only validated fields and the
allowlisted extensions for its exact `ProviderFormat`; it cannot forward a
generic client JSON object to an unrelated provider.

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

Strict typed codecs do not require brittle closed-world schemas. Request and
response types distinguish fields the gateway operates on from bounded raw
extensions, preserve unknown same-format fields, and use explicit
`Unknown(raw)` handling for forward-compatible enum values where safe.
Cross-format conversion remains strict because an unknown construct cannot be
silently translated.

Provider drift is managed operationally as well as through releases:

- Pin provider API versions where the provider permits it and record the
  negotiated/versioned contract in each deployment snapshot.
- Run scheduled and pre-publication canary fixtures against provider sandboxes
  or mocks for required operations, errors, and streaming events.
- Quarantine a deployment automatically when a required response shape or
  capability probe fails; aliases continue only through already conforming
  targets.
- Preserve sanitized unknown response evidence by digest for diagnosis, never
  by logging raw content.
- Track adapter compatibility and provider deprecation dates in GenAI Admin so
  an upgrade can be tested and rolled out before the upstream cutoff.

An alias must fail closed when no eligible deployment can satisfy every
required capability. It must not quietly drop tools, images, JSON Schema, or a
data-residency restriction to make a fallback succeed.

## Control Plane And Model Catalog

The GenAI Admin `LLM Model` area is the authoritative administration surface.
It should present model registration as related objects rather than one large
record that mixes public policy, provider transport, credentials, and dynamic
health.

### Catalog Entities

The initial Portal projection should model these concepts. Exact table and
aggregate names can follow existing Portal conventions, but the boundaries are
contractual.

| Concept | Scope and responsibility |
|---------|--------------------------|
| Model catalog | Platform or host-visible description of a physical provider model: provider type, provider model ID, family/version, lifecycle, context/output limits, modalities, supported operations, and declared capabilities. It contains no credential. |
| Model registration | Host authorization to use a catalog model. It records ownership, allowed environments and regions, data classifications, lifecycle state, and any host-specific capability restriction. |
| Provider deployment | Host-scoped callable endpoint with provider type, physical model ID, base URL, region, transport limits, provider-account/quota-group identity, versioned server-owned credential references, and conformance status. Credentials belong here, never on the agent definition. |
| Public model alias | Client-visible logical name such as `chat-fast-v1`, with allowed operations, required capabilities, token limits, data boundary, logging/PII policy, deprecation, and replacement metadata. |
| Alias route | Ordered or weighted alias-to-deployment relationship with priority, fallback-only status, residency constraints, and rollout/canary policy. |
| Pricing version | Effective-dated rates for input, output, cached input, reasoning, image, audio, service tier, and contract override. Unknown pricing remains explicit. |
| Model policy | Which principals, clients, agents, and product profiles may use an alias, plus budgets, content-logging mode, PII profile, caching, and provider-native extension policy. |

Every mutable tenant record includes `host_id`. Platform-wide reference rows
may be shared read-only, but a shared catalog entry does not grant a host the
right to use it. The host registration, deployment, alias route, and policy
jointly determine eligibility.

The existing `agent_definition_t` directly stores `model_provider`,
`model_name`, and `api_key_ref`. Preserve those fields only for a bounded
migration period. New definitions should reference a public alias or model
policy ID. The existing `agent_model_rate_t` can seed pricing migration, but
runtime accounting must bind a versioned pricing record rather than a mutable
provider/model string pair.

### GenAI Admin Workflow

The `LLM Model` menu should expose focused views over the same aggregates:

- **Catalog** shows technically supported provider models, lifecycle,
  capabilities, context/output limits, modalities, and conformance status.
- **Host registrations and deployments** shows which catalog models the
  selected host may use, regional endpoints, secret references, transport
  bounds, provider account/quota groups, credential lifecycle state, approved
  capacity, and enablement state. Secret values are never displayed.
- **Aliases and routes** edits public names, required capabilities, eligible
  deployments, weights, fallback order, rollout percentage, and data boundary.
- **Pricing and policies** manages effective-dated rates, budgets, allowed
  principals/agents, content mode, caching, and PII profile.

Provide actions to validate a deployment, run its conformance profile, preview
the eligible routes for an alias and sample identity, publish a complete
candidate, inspect its digest/version, and roll back to the last valid version.
Runtime health and recent latency/error observations may be displayed read-only
for operators, but editing or viewing them does not mutate catalog truth.

Agent-definition forms select only authorized public aliases or model policies
for the current `host_id`. They do not offer free-form provider names, physical
model IDs, base URLs, or API-key fields after migration.

### Static And Dynamic Routing Metadata

Portal owns relatively stable, reviewable routing inputs:

- Capabilities and conformance results.
- Context, output, request-byte, and modality bounds.
- Region, residency, data-classification, and provider allowlists.
- Lifecycle, deprecation, replacement, and rollout state.
- Effective-dated price and offline quality/evaluation scores.
- Alias weights, priorities, fallback rules, budgets, and PII/logging profiles.

The gateway owns rapidly changing runtime observations:

- Active/passive health and circuit state.
- Current in-flight work and admission saturation.
- EWMA latency, time to first token, error rate, and rate-limit signals.
- Local lease capacity and recent provider throttling.

Do not update Portal rows for every inference. The gateway combines one
immutable catalog/policy snapshot with local runtime observations, and exports
bounded telemetry asynchronously. Portal may receive aggregated operational
views, but those views are not the routing authority for an in-flight request.

### Publication And Consistency

Catalog changes follow the existing Portal command/event and projection model:

1. Validate aggregate references, host ownership, secret-reference shape,
   capability compatibility, and lifecycle transitions in the command path.
2. Append the control-plane event and project the Portal read model.
3. Build a complete gateway candidate containing provider deployments,
   aliases, routes, capabilities, pricing, and policy digests.
4. Reject an invalid candidate without disturbing the last valid snapshot.
5. Atomically publish the candidate and retain its version/digests in audit and
   agent policy snapshots.

Control-plane resource cardinality should mirror the Portal aggregates: one
changed alias, route, deployment, model policy, or pricing version produces a
small delta rather than republishing every route for a host. Parent resources
should not embed unbounded child lists merely for transport convenience.
However, the gateway's reload worker—not the request path—resolves those
granular resources into affected request-ready plans. It validates all
references, computes effective policy precedence, compiles expressions and
wildcards, and builds provider priority groups. It rebuilds only affected
subgraphs, structurally shares unchanged `Arc` data, and swaps one small
`LlmPublishedSnapshot` root only after the candidate is valid. The root carries
a publication manifest and compatible routing, provider, policy, and pricing
versions, so one atomic load cannot observe a half-published combination.

Pricing may refresh more frequently than model authorization. Publish it as a
separate immutable `PricingSnapshot` subgraph. A pricing-only update creates a
new root pointing to the existing routing/provider/policy subgraphs and the new
pricing `Arc`; it does not rebuild the routing graph. Multiple approved sources
can overlay in declared precedence order; invalid or unreadable updates retain
the last valid snapshot. A public catalog such as models.dev can seed proposed
rates, but an operator-approved effective-dated version remains authoritative
and a price entry never creates a model registration or route.

Rapid health, latency, in-flight, and circuit observations are bounded atomics
owned by stable deployment-runtime objects, not reasons to republish
configuration. Publication metrics include build duration, peak temporary
bytes, reused/rebuilt nodes, active generations, and bytes retained by in-flight
generations. The publisher coalesces superseded updates and stops admitting new
generations when a configured retained-memory bound would be exceeded.

`GET /v1/models` reads the authorized public-alias view from that snapshot. It
does not query Portal or enumerate physical deployments on demand.

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
- Priority groups in which lower-numbered healthy groups are preferred and
  equivalent targets within a group use weight or health/latency score.
- Weighted random across equivalent deployments.
- Least in-flight requests with a bounded weight bias.
- Latency-aware selection using a rolling time-to-first-token and completion
  latency window.
- Cost-aware selection subject to a minimum capability and quality tier.
- Sticky routing by authenticated principal plus bounded session ID.
- Canary and A/B allocation with an auditable stable hash.
- Region and data-boundary routing as hard eligibility rules, not soft weights.
- Conditional virtual aliases evaluated in declaration order over an
  allowlisted context such as authenticated claims, requested operation,
  region, bounded headers, and policy-derived classification. A final explicit
  fallback is required. Raw prompt access is disabled by default because it is
  both sensitive and expensive.

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
- Retain one replayable, bounded provider-neutral request or rendered attempt
  body for pre-output retries. Retry eligibility is explicit for every body
  size and operation; do not silently disable reliability at an arbitrary
  small replay-buffer constant.
- Use a different credential or deployment only when policy allows it.
- Open a circuit after a configurable failure threshold; probe with bounded
  half-open traffic.
- Preserve required capabilities and data-boundary rules across fallback.
- Never begin a fallback after semantic stream output is visible to the client.
- Record each physical attempt separately but charge and report the complete
  logical request accurately.
- Finalize each attempt's health signal before selecting the next target. A
  retryable unhealthy result can eject or penalize that deployment so
  reselection advances to another target or priority group rather than looping
  on the same endpoint.

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

Credential rotation and capacity routing are different features. A deployment
declares one opaque `providerAccountId`, `quotaGroupId`, region, and approved
capacity, and may reference overlapping current/next credential versions for
zero-downtime secret rotation. Every credential in that set inherits the same
quota group, so adding a key cannot increase capacity. A separately approved
provider account/quota is represented as another deployment and alias target.
A `429` can move to that deployment only when policy permits it and the provider
contract treats it as independent authorized capacity; ordinary key rotation
never becomes quota striping.

The gateway must not cycle keys to bypass an upstream RPM/TPM, account tier,
fair-use control, or abuse limit. Such behavior creates financial and provider
account risk and is rejected during configuration review. The 5,000-RPS
performance gate uses a controlled local mock to measure gateway capacity; it
is not an instruction to send 5,000 RPS through one or many production
provider keys.

### Guardrails And Data Protection

Support ordered pre-provider and post-provider policy hooks:

- Prompt and attachment size/type validation.
- PII/tokenization or DLP policy.
- Moderation and prohibited-content policy.
- Prompt-injection and secret-exfiltration signals where configured.
- Tool schema and tool-name allowlists.
- Structured-output schema validation.
- Output redaction and data-boundary checks.

Compile the effective hook order into the alias plan. The default normalized
request order is: validate client fields; apply server-owned defaults and
prompt enrichment; run local request guardrails and PII classification;
tokenize protected spans; reserve the final token/cost bound; select an
eligible target; apply only that target's typed provider transformation and
authentication; then dispatch. Provider-specific remote guardrails declare
whether they receive original, tokenized, or metadata-only content, and policy
validation rejects a data-boundary violation before activation.

On response, decode the provider format into semantic events before applying
content policy. Exact placeholder recovery runs only for the originating
authorized scope. Local safety policy can run before or after recovery as its
profile declares; a remote service receives recovered cleartext only when its
data boundary explicitly permits it. Finally render the original
`ClientFormat` and release buffered or approved streaming frames.

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

Content logging is separately authorized, sampled, redacted or tokenized,
encrypted, and retention-bounded. Never put prompts, completions, tool
arguments, user IDs, API keys, or model output into metric labels.

Content is not a general-purpose handler attribute. Materialize prompt,
completion, tool, or raw-body views lazily only when the compiled policy proves
that an authorized content hook needs them. Make the raw request available only
inside that content-policy phase and remove it before general transformations,
access logs, and telemetry expressions run. Later stages receive normalized
metadata, policy outcomes, digests, or encrypted object references.

Parsing and provider-error paths follow the same rule. Ordinary logs never
include raw request bodies, provider response/error bodies, malformed SSE data,
or a "first N bytes" preview. Emit the normalized error category, provider and
route IDs, status, content type, byte length, parser position, and a keyed
digest. An authorized encrypted-content capture is an audit operation with its
own purpose and retention, not a debug log statement.

#### Storage Ownership

Use storage according to the data's workload and security domain:

| Data | Authoritative home | Notes |
|------|--------------------|-------|
| Model catalog, aliases, routes, policies, and pricing | Light Portal PostgreSQL | Control-plane data projected to immutable gateway snapshots. |
| Canonical agent conversation/action events | Portal agent event ledger | Bounded normalized event or content reference used to rebuild agent state; not a copy of every physical provider attempt. |
| Logical inference request and physical attempt metadata | Dedicated local/regional audit PostgreSQL | Append-oriented, time-partitioned, separately credentialed, and written asynchronously. |
| Authorized prompt/response bodies and multimodal objects | Encrypted content store | Store ciphertext or an immutable object-store reference plus digest; do not put large content in Portal OLTP rows. |
| Delivery backlog after sink interruption | Gateway-local bounded WAL/spool | Store-and-forward only. It is not the only copy after acknowledgement and is not queried as the audit corpus. |
| Immediate reversible PII mapping | Request memory | Default for synchronous inference; destroy after the response and audit finalization. |
| Durable reversible PII mapping | Separate regional PII vault | Used only for asynchronous, multi-turn, restart/failover, or explicit retention requirements. Never colocate with the audit content corpus. |

Here, "local" means within the organization's approved trust zone or region.
It does not mean that each gateway replica owns the only durable copy. An
embedded database such as SQLite is suitable for a single-writer spool, but
replica loss, rescheduling, failover, and cross-replica queries make it a poor
authoritative audit or PII store.

#### Audit Record Model

Represent one client call separately from its physical provider attempts:

- Proposed `llm_request_t` is the time-partitioned logical-request table.
- Proposed `llm_attempt_t` is the ordered physical-attempt table keyed to the
  logical request and partition period.
- Proposed `llm_content_object_t` stores only encrypted-object metadata,
  immutable reference, digest, media type, size, encryption-key reference,
  retention class, and deletion state.
- Proposed `llm_dataset_export_t` records a curated audit/evaluation/training
  export manifest, purpose, approvals, transformations, source partition range,
  content digests, and retention/deletion state.

At minimum, those records contain:

- A logical request record contains request/correlation IDs, authenticated
  host/client/agent identities or opaque references, public alias, operation,
  policy/catalog/config versions, admission and completion timestamps, final
  status, usage/cost totals, retention class, content mode, PII profile, and
  content references/digests.
- An attempt record contains logical request ID, attempt number, internal route
  and deployment IDs, physical model, retry/fallback reason, provider request
  ID, connect/provider/first-token/total timing, status, cancellation state,
  provider usage evidence, and pricing version.
- A transformation manifest records whether request content was raw,
  tokenized, redacted, or omitted at provider dispatch and audit capture. It
  stores policy and detector versions plus digests, not reversible cleartext.

This distinction preserves evidence when a single logical request retries,
falls back, times out after provider acceptance, or returns a partial stream.
Streaming produces one summarized attempt record rather than one database row
per chunk.

#### Content Modes And Purpose

Each resolved model policy selects one explicit content mode:

- `metadata-only`: default; store no prompt, completion, or tool content.
- `tokenized-content`: store the provider-visible tokenized exchange for
  approved audit/evaluation use.
- `encrypted-raw`: exceptional; store envelope-encrypted pre-tokenization or
  post-recovery content under stricter authorization and shorter retention.
- `disabled`: emit only the minimum operational counters allowed by policy and
  no durable request-level audit record where regulations require that mode.

Audit, evaluation, and training are different purposes. Operational audit data
does not automatically become training data. A separately authorized export
job creates a versioned, immutable dataset manifest, applies consent and
retention rules, records source digests and transformations, and excludes data
that is not approved for the requested purpose.

#### Delivery And Failure Semantics

Audit policy separates admission pressure from crash durability. A model
policy selects one of these explicit profiles; `required` by itself must never
be interpreted as an unspecified durability promise:

| Profile | Before provider dispatch | Crash guarantee | Intended use |
|---------|--------------------------|-----------------|--------------|
| `best-effort` | Try to enqueue; pressure may drop the record and increments a loss counter. | None. | Local development only; invalid for an alias requiring audit. |
| `bounded-async` | Reserve a complete bounded envelope and queue/spool budget or fail admission. The request does not wait for a disk commit. | A declared tail window can be lost if the process or node fails before the writer commits it. | Default metadata-only, high-throughput production profile and the feature-equivalent Bifrost comparison. |
| `local-durable` | Append the admitted/attempt-start event and wait for the WAL durable watermark before every provider attempt. | A crash can leave an explicitly incomplete attempt, but cannot erase evidence that dispatch was authorized. | Regulated workloads that require pre-dispatch evidence. It has a separately reported latency SLO. |
| `remote-durable` | Wait for an idempotent authoritative-sink transaction. | Survives loss of the gateway node according to the sink's durability contract. | Later strict-accounting profile; not part of the MVP fast path. |

For `bounded-async` and `local-durable`, admission reserves the worst-case
metadata budget for the logical request and configured maximum attempts before
expensive parsing or dispatch. If the queue and allowed spool path cannot honor
that reservation, fail before provider work. Optional content capture may be
dropped independently while retaining required metadata.

The MVP spool is a single-writer, append-only segmented WAL, not an embedded
query database. Its stable on-disk format is:

- A segment header containing a fixed Light LLM-audit magic value, WAL format
  version, segment UUID, gateway-instance ID, and creation timestamp.
- Immutable records encoded as `[length][checksum][sequence][payload]`.
  `length` is bounded, `checksum` covers the sequence and payload, and a corrupt
  or partial tail is truncated during recovery.
- A UTF-8 JSON payload with its own `schemaVersion`, UUIDv7 `eventId`, logical
  request ID, optional attempt number, event kind, timestamp, published-snapshot
  digest, and metadata body. MVP WAL payloads never contain prompts,
  completions, tool arguments, provider error bodies, credentials, or PII.
- Event kinds `request_admitted`, `attempt_started`, `attempt_finished`, and
  `request_finished`. Records are append-only; completion never overwrites a
  start record.

Serialization evolution is additive within a payload schema version. A reader
must skip a bounded unknown event kind, but it must reject an unknown segment
format version rather than guessing record boundaries. Segment and record size
limits are validated before publication.

A dedicated writer batches by maximum records, bytes, and commit delay. In
`local-durable` mode it calls `fdatasync` after the batch and advances a
monotonic durable-sequence watermark. A request waiting to dispatch succeeds
only when its start sequence is at or below that watermark; timeout, I/O error,
read-only filesystem, or a full volume fails the request without an upstream
attempt. This is bounded group commit, not per-request file opening or an
unbounded synchronous database call.

For buffered `local-durable` requests, `request_finished` is also committed
before a successful final response when `terminalCommitBeforeResponse` is
enabled. For SSE, `attempt_started` is durable before response headers or the
first semantic event; the terminal event is appended on normal completion. A
crash after streaming begins therefore leaves a durable incomplete attempt
rather than falsely recording success.

The sink consumes WAL events in sequence and writes them idempotently using the
unique `eventId` plus logical request/attempt keys. A segment is deleted only
after every record has an authoritative acknowledgement and the acknowledgement
checkpoint is itself durable. Startup scans and verifies segments, truncates
only a partial final record, replays unacknowledged events, and marks a durable
start without a terminal event as incomplete. Duplicate delivery is expected
and must not create duplicate request or attempt rows.

`local-durable` is valid only with a dedicated persistent volume whose
deployment contract survives process and pod restart. Configuration must
declare the persistence class; an ephemeral `emptyDir`, container filesystem,
or undocumented host path is rejected for this profile. The directory is
gateway-write-only, uses restrictive file permissions and encrypted storage,
and has explicit capacity, retention, and alert thresholds. Node loss beyond
the volume's durability boundary requires `remote-durable` rather than a
stronger claim about a local WAL.

A background sink batches metadata into the dedicated audit database and
places later authorized large encrypted content in the content store. It never
performs an unbounded synchronous Portal or audit-database write on the normal
high-throughput path.

Partition metadata by time and make retention removal a partition operation.
Encrypt content with per-host or per-retention-class data keys, keep key
references out of content rows, and use separate roles for gateway writes,
auditor reads, dataset export, and deletion.

### Reversible PII Tokenization

Reversible PII tokenization is feasible, but the LLM path needs content-aware
token processing rather than only JSON-field replacement.

The existing `light-pingora` PII handler is a useful cryptographic and schema
baseline: it scopes lookup by `host_id`, encrypts cleartext values, stores a
keyed value hash, and can tokenize configured request fields and detokenize
configured response fields. It is not the final LLM implementation because:

- It replaces the complete string at a configured JSON path; it does not find
  multiple sensitive substrings inside a normal message or tool argument.
- Response detokenization expects a field value to be exactly one stored token;
  it does not recover authenticated placeholders embedded in model prose.
- Response transformation buffers the complete body, which is incompatible
  with transparent SSE streaming.
- The current insert path does not assign an expiry, host-stable value reuse is
  linkable across requests, and the default cache can retain cleartext.
- Its database URL can fall back to the shared application database, which is a
  deployment convenience rather than the desired production security boundary.

#### Policy And Transformation Flow

The resolved model policy includes a `piiProfileId`, token scope, detector and
rule versions, allowed data classifications, failure mode, and whether
streaming remains eligible. Apply it in this order:

1. Normalize the client request and identify eligible message text, text
   content parts, structured fields, and tool arguments. Do not tokenize
   provider routing, schema names, tool names, or control metadata accidentally.
2. Detect configured PII types using compiled local rules or a bounded detector
   profile. A remote DLP dependency is a separately named strict profile with
   its own admission and latency SLO.
3. Replace each sensitive span with a high-entropy authenticated placeholder,
   using a short fixed ASCII grammar such as
   `[LPII1_<base32-id>_<truncated-mac>]`, and keep the mapping in the request
   context by default. The exact grammar and tag length follow a security
   review; the example is illustrative.
4. Serialize and send only the tokenized canonical request to a cloud provider.
5. Scan normalized response text and tool arguments for exact placeholders,
   validate the MAC and host/request/session scope, and recover authorized
   values before returning the response to the originating agent.
6. Record transformation digests and policy versions in audit metadata. Store
   content only according to the resolved content mode.
7. Destroy request-scoped cleartext mappings after response delivery, audit
   finalization, and any required retry window.

Never use fuzzy token recovery. A missing, expired, altered, or unauthorized
placeholder remains masked or fails the response according to policy. It must
never trigger a broader lookup or reveal a value from another host, principal,
request, or session.

Exact recovery is a security invariant, but a mangled token does not need to
make the normal user experience brittle. Tokenization profiles therefore also
define `unresolvedTokenPolicy`:

- `leave-masked` is the default. Preserve the unresolved placeholder or replace
  an identifiable malformed token with a generic irreversible marker, record a
  near-miss outcome, and continue. Never guess the original value.
- `reject-buffered` is for workflows that require complete recovery. It forces
  buffered output and rejects before any semantic bytes are emitted.
- A streaming profile cannot select a policy that would fail the response
  after earlier content has reached the client.

The gateway adds a short server-owned instruction to copy placeholders
verbatim when the alias permits prompt enrichment, and keeps placeholders in
structured content/tool values where possible. Provider conformance measures
exact preservation, alteration, omission, and hallucinated-token rates over a
versioned corpus. A model/deployment that does not meet the profile threshold
is ineligible for reversible PII; the alias must use irreversible redaction,
buffered strict handling, or another deployment. Near-match detection may
produce telemetry, but it never performs a vault lookup or detokenization.

#### Token Scope And Vault Boundary

Support these scopes explicitly:

- `request`: default. The mapping exists only in request memory and avoids
  database I/O on the normal inference path.
- `session`: opt-in for multi-turn tokenized history. Mappings expire with a
  bounded session retention and are available to authorized gateway replicas.
- `host`: exceptional stable-token mode for a documented integration need. It
  increases linkability and requires explicit security approval.

A durable mapping is required for asynchronous/batch inference, multi-turn
history containing placeholders, restart/failover recovery, or a response that
may resume on another replica. Access it through a narrow `PiiVault` interface:
insert with expiry, resolve one exact scoped token, revoke, and expire. The
gateway role cannot scan or export the vault.

The production implementation can be a dedicated regional PostgreSQL vault, a
vault service, or a Redis-compatible distributed KV deployment only when it
meets the same security and durability contract: independent credentials and
network boundary, TLS, encryption of values with external key references,
atomic insert/resolve semantics, enforced TTL, bounded memory behavior,
restart/failover persistence, backup/recovery objectives, access audit, and
tested deletion. An ordinary volatile Redis/Dragonfly cache or an eviction
policy that can discard live mappings is not a durable PII vault. PostgreSQL is
the conservative durable default; a qualified KV implementation is an optional
session-scale profile selected by measured latency and recovery requirements.

The durable vault entry includes at least host ID, opaque token ID, token
format/version, scope kind, a non-reversible scope binding, PII type, encrypted
value, nonce, key ID, creation/expiry timestamps, and active/deletion state.
Request scope does not use the current host-stable `value_hash` uniqueness
rule. Session/host deduplication, when explicitly required, uses a separate
keyed hash and policy so linkability is visible and reviewable.

Using a separate schema and role in the Portal PostgreSQL cluster is an
acceptable transition for development or an initial low-risk deployment, but
it shares administrator, backup, and failure boundaries. Production PII
mappings should not live in the Portal database, an audit-content database, or
a replica-local embedded database. Colocating reversible mappings with logged
content would defeat the intended breach separation.

#### Streaming Recovery

Provider streams can split a placeholder across arbitrary SSE and UTF-8 chunk
boundaries. A streaming-compatible PII profile keeps a bounded suffix no larger
than the maximum placeholder length, emits only bytes that cannot begin a
placeholder, and validates/replaces complete placeholders before release.
An altered candidate follows `leave-masked`; it is not recovered fuzzily and
does not terminate a normal stream after earlier semantic output. A strict
`reject-buffered` profile is never published as streaming-compatible.

If detection or response policy requires whole-message context, force buffered
mode or reject streaming for that alias. Do not emit raw partial token syntax
and attempt to retract it later. Tokenization and recovery benchmarks must be
reported as named policy profiles; the metadata-only/no-PII profile remains the
baseline performance contract.

## Token And Cost Governance

Usage accounting must distinguish estimated, provider-reported, and locally
counted values.

When a provider lacks a native token-count endpoint, a local tokenizer may
answer an internal estimate or a later provider-native compatibility endpoint.
The result is labelled `local-estimate` with tokenizer/model-table version; it
can enforce a conservative admission bound but cannot be recorded as exact
provider billing evidence. A native count response is also not inference usage
and must not be charged as consumed prompt tokens.

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

Provider-side capacity is tracked by declared `providerAccountId` and
`quotaGroupId`, not merely credential ID. Local admission consumes RPM/TPM and
concurrency leases for that group before dispatch and updates them from
provider rate-limit signals. Adding or rotating another secret in the same
group does not create more capacity.

Opaque routes use request/byte/duration/concurrency units plus the configured
fixed cost envelope and account-spend ceiling. Because realized token usage is
unknown, reconciliation cannot release a pessimistic reservation based on a
guess; only authoritative later billing evidence may amend it. This makes
opaque compatibility intentionally less efficient than normalized routing
rather than a way around financial governance.

The pricing catalog must be versioned and support:

- Input, output, cached input, and cache-creation rates.
- Reasoning, image, audio, and other provider-specific billable units.
- Tiered context pricing and provider service tiers.
- Contract-specific overrides.
- A clear `unknown` state. Unknown pricing must not silently become zero when a
  hard cost budget is configured.
- Atomic replacement, declared source precedence, and last-valid retention on
  refresh failure. The logical request and every physical attempt capture the
  exact pricing snapshot and effective tier used for reconciliation.

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

Its lookup is an explicit sub-operation in the request deadline and budget,
not hidden preprocessing:

1. Perform normal identity, alias, size, data-boundary, and fail-fast global
   admission first; check the cheaper exact cache before semantic work.
2. Acquire separate bounded semantic-cache and embedding permits. Reserve the
   embedding token/cost envelope and a configured deadline slice without
   consuming all time needed for the primary LLM route.
3. Call only a policy-approved embedding deployment, then run a bounded vector
   lookup. Account for both operations and audit their model/index versions.
4. On a hit, apply current authorization and post-response policy before
   returning. On a miss, release cache permits and continue with the remaining
   LLM deadline and budget. A lookup timeout follows the alias's explicit
   `cacheFailureMode`, normally `treat-as-miss`; it never waits without bound.
5. Coalesce identical in-flight cache fills and bound fill concurrency so a
   popular miss cannot create an embedding or provider stampede.

Client end-to-end time to first byte starts at gateway admission and therefore
includes semantic lookup. Report `semantic_embedding_duration`,
`semantic_lookup_duration`, `time_to_cache_hit`, and the later provider
time-to-first-token separately. A cache hit has no provider TTFT. Release and
performance gates use named semantic-cache profiles rather than hiding this
latency inside routing.

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

admission:
  maxQueuedRequests: ${llm-router.admission.maxQueuedRequests:0}
  maxQueueWaitMs: ${llm-router.admission.maxQueueWaitMs:0}
  overloadStatus: ${llm-router.admission.overloadStatus:503}
  streamBufferEvents: ${llm-router.admission.streamBufferEvents:32}
  streamSetupTimeoutMs: ${llm-router.admission.streamSetupTimeoutMs:5000}
  maxStreamLifetimeMs: ${llm-router.admission.maxStreamLifetimeMs:120000}
  downstreamWriteProgressTimeoutMs: ${llm-router.admission.downstreamWriteProgressTimeoutMs:10000}
  minStreamDrainBytesPerSecond: ${llm-router.admission.minStreamDrainBytesPerSecond:256}

publication:
  maxRetainedGenerations: ${llm-router.publication.maxRetainedGenerations:8}
  maxRetainedBytes: ${llm-router.publication.maxRetainedBytes:536870912}
  coalesceWindowMs: ${llm-router.publication.coalesceWindowMs:100}

opaqueDefaults:
  maxRequestBytes: ${llm-router.opaqueDefaults.maxRequestBytes:1048576}
  maxResponseBytes: ${llm-router.opaqueDefaults.maxResponseBytes:8388608}
  maxDurationMs: ${llm-router.opaqueDefaults.maxDurationMs:30000}
  fixedCostReservationUsd: ${llm-router.opaqueDefaults.fixedCostReservationUsd:}
  requireAccountSpendCeiling: ${llm-router.opaqueDefaults.requireAccountSpendCeiling:true}

telemetry:
  auditQueueCapacity: ${llm-router.telemetry.auditQueueCapacity:8192}
  usageQueueCapacity: ${llm-router.telemetry.usageQueueCapacity:8192}
  perChunkEvents: ${llm-router.telemetry.perChunkEvents:false}

audit:
  admissionPolicy: ${llm-router.audit.admissionPolicy:required}
  durability: ${llm-router.audit.durability:bounded-async}
  contentMode: ${llm-router.audit.contentMode:metadata-only}
  includeProviderRequestId: ${llm-router.audit.includeProviderRequestId:true}
  contentSampleRate: ${llm-router.audit.contentSampleRate:1.0}
  terminalCommitBeforeResponse: ${llm-router.audit.terminalCommitBeforeResponse:true}
  spool:
    enabled: ${llm-router.audit.spool.enabled:true}
    path: ${llm-router.audit.spool.path:/var/lib/light-gateway/llm-audit}
    persistenceClass: ${llm-router.audit.spool.persistenceClass:ephemeral}
    maxBytes: ${llm-router.audit.spool.maxBytes:1073741824}
    segmentBytes: ${llm-router.audit.spool.segmentBytes:67108864}
    maxRecordBytes: ${llm-router.audit.spool.maxRecordBytes:65536}
    maxBatchRecords: ${llm-router.audit.spool.maxBatchRecords:256}
    maxBatchBytes: ${llm-router.audit.spool.maxBatchBytes:1048576}
    maxCommitDelayMs: ${llm-router.audit.spool.maxCommitDelayMs:1}
    commitTimeoutMs: ${llm-router.audit.spool.commitTimeoutMs:10000}
  sink:
    type: ${llm-router.audit.sink.type:postgres}
    databaseUrl: ${llm-router.audit.sink.databaseUrl:}
    batchSize: ${llm-router.audit.sink.batchSize:256}
  contentStore:
    type: ${llm-router.audit.contentStore.type:none}
    bucket: ${llm-router.audit.contentStore.bucket:}

pii:
  defaultProfile: ${llm-router.pii.defaultProfile:none}
  vault:
    type: ${llm-router.pii.vault.type:none}
    url: ${llm-router.pii.vault.url:}
    credentialRef: ${llm-router.pii.vault.credentialRef:}
    durabilityProfile: ${llm-router.pii.vault.durabilityProfile:durable}
    maxConnections: ${llm-router.pii.vault.maxConnections:8}
    defaultTtlSeconds: ${llm-router.pii.vault.defaultTtlSeconds:86400}
  profiles:
    - id: cloud-request-scoped
      scope: request
      detector: local-rules-v1
      tokenFormat: authenticated-placeholder-v1
      unresolvedTokenPolicy: leave-masked
      streamingMode: bounded-token-window

providers:
  openai-primary:
    type: openai
    baseUrl: ${llm.providers.openaiPrimary.baseUrl:https://api.openai.com/v1}
    providerAccountId: openai-account-primary
    quotaGroupId: openai-tier-primary
    credentials:
      - id: openai-key-current
        secretRef: ${llm.providers.openaiPrimary.secretRef:}
        lifecycle: current
    connectTimeoutMs: 3000
    requestTimeoutMs: 90000
  anthropic-primary:
    type: anthropic
    baseUrl: ${llm.providers.anthropicPrimary.baseUrl:https://api.anthropic.com}
    providerAccountId: anthropic-account-primary
    quotaGroupId: anthropic-tier-primary
    credentials:
      - id: anthropic-key-current
        secretRef: ${llm.providers.anthropicPrimary.secretRef:}
        lifecycle: current
    connectTimeoutMs: 3000
    requestTimeoutMs: 90000

models:
  - name: chat-fast-v1
    clientFormats: [openai-chat-completions]
    operations: [chat]
    processingMode: normalized
    capabilities: [streaming, tools, vision]
    maxInputTokens: 128000
    maxOutputTokens: 8192
    dataClassifications: [public, internal]
    routes:
      - id: openai-fast-primary
        provider: openai-primary
        model: provider-physical-model-a
        providerFormat: openai-chat-completions
        priority: 0
        weight: 80
        regions: [ca, us]
      - id: anthropic-fast-fallback
        provider: anthropic-primary
        model: provider-physical-model-b
        providerFormat: anthropic-messages
        priority: 1
        weight: 100
        regions: [ca, us]

routing:
  strategy: priority-weighted
  maxAttempts: 3
  baseBackoffMs: 100
  maxBackoffMs: 2000
  retryStatuses: [408, 429, 500, 502, 503, 504]
  circuitBreaker:
    failureThreshold: 5
    resetTimeoutMs: 30000
    halfOpenRequests: 1
  sessionHeader: X-Light-Session-Id
```

This is a proposed shape, not a statement that these keys are already
implemented. Portal source records keep secret-bearing deployments, public
aliases/routes, model policies, audit-sink configuration, and PII profiles as
separate security and lifecycle objects. `llm-router.yml` is their validated
data-plane projection, not a second independently edited source of truth.

Configuration validation must reject:

- Duplicate alias, provider, route, or deployment IDs.
- Empty credential sets for an enabled provider unless its authentication mode
  explicitly allows them.
- Deployments without provider-account/quota-group identity, invalid credential
  lifecycle overlap, or a capacity policy that would cycle keys inside one
  quota group to bypass upstream limits.
- Invalid or unsafe provider base URLs.
- Fallback targets missing an alias's required capability or region.
- A route whose `ProviderFormat` cannot represent the alias's `ClientFormat`
  and operation without dropping a required field or event.
- `detect` or `opaque` processing on an alias that requires content
  guardrails, reversible PII, normalized audit, cross-format fallback,
  authoritative token accounting, or exact realized per-call cost accounting.
- An opaque route without bounded bytes, duration, request/concurrency limits,
  and either a pessimistic fixed cost reservation or an authoritative
  provider-account spend ceiling.
- Routes to interactive providers in a shared profile.
- Impossible token or timeout bounds.
- Unknown strategy, operation, capability, or unsupported-parameter policy.
- Unbounded or contradictory admission settings, including a non-zero queue
  without a positive queue-wait deadline.
- Stream admission without positive setup, write-progress, idle, and absolute
  lifetime deadlines, or publication without retained-generation and
  retained-memory bounds.
- Unbounded audit, usage, cache, stream-event, or provider concurrency buffers.
- An alias that requires audit selecting `best-effort`, or an unknown
  admission/durability profile.
- `local-durable` without a bounded WAL, positive commit timeout, valid
  segment/record/batch limits, and a declared persistent-volume class.
- `remote-durable` without an authoritative idempotent sink and a bounded
  transaction timeout.
- An alias or model policy referencing a missing/inactive deployment, pricing
  version, content mode, or PII profile.
- A durable PII profile without a separately credentialed vault, expiry, key
  reference, and exact host/session authorization policy.
- Required audit admission without a complete worst-case envelope reservation,
  or `encrypted-raw` content mode without an encrypted content store and
  retention class.
- `disabled` audit selected by a policy that requires a durable request record,
  or sampling applied to required metadata instead of optional content.
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
- Portal-backed host model registrations, deployments, aliases, model policies,
  pricing versions, and an atomic gateway projection.
- Server-side credentials with masking and base-URL validation.
- Existing Light authentication, access control, correlation, metrics, and
  request rate limits.
- Request, token, response, timeout, concurrency, and attempt bounds.
- Typed errors and explicit unsupported-parameter behavior.
- Separate client/operation/provider-format types, with a same-format
  compatibility fast path and no parse-failure downgrade to opaque forwarding.
- Provider conformance tests for the first supported deployment set.
- Provider API-version pinning where available, forward-compatible same-format
  envelopes, and deployment quarantine on required contract drift.
- Metadata-only logical-request and physical-attempt audit events, normalized
  usage, `bounded-async` delivery for the default performance profile, and a
  separately measured `local-durable` WAL profile feeding a dedicated audit
  sink.
- Checked-in direct/mock/Bifrost benchmark harness, immutable benchmark
  manifests, and passing absolute and comparative performance gates.
- One wait-free structurally shared published-snapshot root, reusable provider
  clients, fail-fast bounded admission, bounded streaming/audit/usage channels,
  and slow-consumer setup/write-progress/absolute deadlines.

### Production Hardening

- Weighted and least-in-flight routing.
- Health/latency-scored priority groups and retry-driven outlier reselection.
- Per-target circuit breakers and active/passive health signals.
- Shared per-principal token, cost, and concurrency budgets.
- Provider-account/quota-group capacity, governed credential lifecycle, and
  explicit prohibition of key cycling for limit evasion.
- Versioned pricing and usage reconciliation.
- Model/region/data-boundary policy.
- Structured outputs and expanded multimodal conformance.
- Exact response caching.
- Pre/post guardrail hooks and strict streaming policy modes.
- Policy-selected tokenized/encrypted content capture with purpose-specific
  retention and curated evaluation/training dataset export.
- Request-scoped LLM content tokenization and exact response recovery, followed
  by a separately credentialed regional PII vault for session, asynchronous,
  and failover profiles.
- Portal configuration, route inspection, usage, and budget views.
- OpenTelemetry traces and operational dashboards.
- Multi-replica chaos, failover, and stream soak testing.
- Continuous capacity-regression testing for the production handler profile,
  including allocation, CPU, RSS, connection reuse, and overload recovery.

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
- Slow-consumer write-progress/drain-rate terminations and stream permit hold
  time.
- Snapshot publication build/retire duration, rebuilt/shared nodes, retained
  generations, and retained bytes.
- PII placeholder preservation, alteration, omission, hallucination, unresolved
  handling, and recovery outcomes without token IDs or values as labels.
- Semantic embedding/lookup duration, time to cache hit, coalesced fills, and
  cache-failure-mode outcomes.

Labels can include public alias, operation, route ID, provider type, status
class, and environment when their value sets are bounded. Never label metrics
with prompt text, user-provided model strings, user IDs, session IDs, raw
provider error text, or provider request IDs.

Each trace should separate:

- Authentication and policy evaluation.
- Quota reservation.
- Exact-cache lookup and, when enabled, semantic embedding/vector lookup.
- Route selection.
- Each provider attempt.
- First-token wait and stream transfer.
- Guardrail processing.
- Usage and cost reconciliation.

Tracing is sampled and request-scoped. The default profile does not create a
span or export event for every stream chunk. High-detail diagnostic tracing is
time-bounded, rate-limited, and excluded from release benchmark comparisons
unless enabled identically for every candidate.

## Testing Strategy

### Protocol Tests

- Golden request/response fixtures from current OpenAI SDKs.
- Chat message roles, content parts, tool calls, structured output, usage, and
  error envelopes.
- SSE fragmentation at arbitrary byte boundaries, multi-byte UTF-8, tool-call
  argument deltas, final usage, `[DONE]`, and midstream failure.
- SDK smoke tests with at least Python and TypeScript clients using only a base
  URL and credential change.
- Same-format unknown-field preservation, operated-field validation, and proof
  that cross-format conversion rejects unknown extensions it cannot represent.
- Forward-compatible unknown enum/field fixtures prove safe same-format
  preservation while required or unsafe drift quarantines the deployment.
- Proof that an internally requested streaming usage record is stripped when
  the client did not request `stream_options.include_usage`.

### Provider Contract Tests

- Provider mock servers for every supported success and error shape.
- Capability conformance by physical model/deployment.
- Authentication, rate-limit, invalid-request, context-limit, timeout, `5xx`,
  malformed JSON, oversized body, and truncated stream behavior.
- Provider request ID, `Retry-After`, usage, and cancellation extraction.
- Secret redaction from every error and log path.
- Malformed JSON/SSE and provider parse failures prove that raw body prefixes,
  prompts, completions, tool arguments, and provider error bodies never reach
  ordinary logs or traces.

### Routing And Governance Tests

- Deterministic alias resolution from one immutable published root and its
  generation-compatible sub-snapshots.
- Host registration, deployment, alias-route, model-policy, pricing-version,
  and agent-definition referential validation.
- Proof that a shared catalog row does not grant an unregistered host access
  and that `/v1/models` never exposes physical deployments.
- Capability, policy, region, and data-boundary target filtering.
- Weighted distribution and stable canary/session allocation.
- Retry/fallback deadline and attempt limits.
- Priority-group failover proves that a retryable unhealthy response updates
  outlier state before reselection and does not choose the same ejected target.
- Replay tests cover requests above 64 KiB up to the configured maximum and
  prove that retry eligibility is explicit rather than silently disabled.
- Proof that no retry or fallback starts after the first semantic stream event.
- Atomic token/cost reservation under concurrency and correct reconciliation on
  success, error, timeout, and cancellation.
- Cross-tenant cache and session-stickiness isolation.
- Credential-rotation tests prove keys in one quota group share capacity and a
  `429` cannot trigger quota-evasion cycling; independent approved accounts can
  fail over only according to explicit policy.
- Opaque routes enforce byte/request/duration/concurrency and fixed-cost/account
  ceilings, mark token/realized cost unknown, and cannot satisfy a normalized
  alias accidentally.

### Runtime Tests

- Candidate config rejection leaves the previous runtime active.
- Config reload does not mutate in-flight route snapshots.
- Pricing-only and one-alias publications structurally share unchanged
  subgraphs, preserve a generation-consistent root, coalesce rapid updates, and
  stay within retired-generation/byte bounds under long-lived requests.
- High-concurrency buffered and streaming load tests.
- Slow-client, disconnect, provider-stall, circuit-breaker, and provider-outage
  chaos tests.
- Slowloris tests trickle request bytes and downstream reads, send heartbeats,
  and hold streams across setup/idle/write-progress/absolute deadlines; every
  path cancels upstream and recovers per-principal/global permits.
- Multi-replica budget and cache tests against the selected shared stores.
- Metrics cardinality and content-leak checks.

### Audit And PII Tests

- One logical audit record with all ordered physical attempts for retry,
  fallback, timeout-after-acceptance, partial stream, cancellation, and cache
  hit paths.
- Required-audit admission failure when the complete logical-request/attempt
  envelope cannot be reserved; `best-effort` is rejected for a required alias.
- WAL fixtures cover versioned segment headers, record length/checksum/sequence,
  maximum record and segment sizes, partial-tail truncation, mid-segment
  corruption rejection, and unknown payload event kinds.
- `local-durable` proves that no provider mock observes a request before the
  corresponding start sequence reaches the durable watermark. Commit timeout,
  `fdatasync` failure, read-only/full volume, and writer termination all fail
  before dispatch and release reservations.
- Recovery replays unacknowledged events without duplicate rows, preserves a
  durable start as an incomplete attempt when no terminal event exists, and
  deletes a segment only after a durable authoritative acknowledgement.
- `bounded-async` tests and telemetry expose its documented crash-loss window;
  they never label an in-memory reservation as durable.
- Time partition creation/retention, encrypted content references, key/role
  isolation, purpose-specific dataset export, and deletion evidence.
- PII detection and replacement inside message text, content-part arrays, and
  tool arguments, including multiple values and repeated values.
- Exact authenticated-token recovery, expiry, request/session/host scoping,
  unresolved-token policy, and proof that forged or cross-host tokens never
  reveal cleartext.
- A versioned per-model preservation corpus measures exact, altered, omitted,
  and hallucinated placeholders; `leave-masked` never guesses or fails a
  partially emitted stream, while `reject-buffered` emits no partial content.
- Placeholder fragmentation at every streaming byte boundary, including UTF-8
  boundaries, slow consumers, cancellation, and maximum suffix-buffer bounds.
- Request-scoped profiles perform no vault I/O and destroy cleartext state;
  durable profiles survive the documented multi-replica failover cases.
- Every `PiiVault` implementation passes the same TTL, restart/failover,
  eviction, encryption, authorization, backup/restore, audit, and deletion
  contract; a volatile cache fails the durable profile.
- Content-mode tests prove that `metadata-only`, `tokenized-content`,
  `encrypted-raw`, and `disabled` produce only the authorized records.

### Performance And Capacity Tests

- A direct mock-provider baseline plus Light and pinned-Bifrost runs generated
  from one versioned manifest.
- Open-loop offered-load sweeps that locate the sustainable capacity knee and
  verify prompt load shedding above it.
- The 500-RPS, 5,000-RPS, production-handler, streaming, overload, large-body,
  and cold-start profiles defined by the release performance gates.
- Five or more steady-state repetitions with raw histograms, confidence
  intervals, CPU, RSS, allocation, connection, queue, and task telemetry.
- Assertions that one request captures one published root, does not build an
  HTTP client, performs no synchronous control-plane I/O, and creates no
  disabled-hook task.
- Assertions that the request path takes no control-plane/configuration
  `RwLock`, performs no policy-map merge, and does not resolve a provider by
  string. Measure normalized conversion and same-format compatibility paths
  separately.
- Static-enum and preconstructed dynamic provider dispatch run under identical
  5,000-RPS/allocation profiles; dispatch is not standardized until the
  confidence interval demonstrates whether either is materially better.
- Snapshot-churn profiles vary pricing and alias publication rates while
  measuring build CPU, peak temporary/retained memory, generation retirement,
  allocator behavior, and request P99.
- Named benchmark profiles for metadata-only audit, tokenized content,
  encrypted-raw content, request-scoped PII, durable-vault PII, and buffered
  whole-response policy. Do not average strict-policy remote I/O into the
  default fast-path result.
- Run metadata-only audit separately as `bounded-async` and `local-durable`.
  The former remains enabled in the production-handler comparison; the latter
  reports WAL batch/commit-wait histograms and never dispatches before its
  durable watermark.
- Slow-provider and slow-client tests proving bounded queues, cancellation,
  permit release, and recovery without a latency backlog.
- Semantic-cache profiles include embedding and vector lookup in end-to-end
  latency/cost, exercise timeout-as-miss and stampede coalescing, and report
  cache-hit latency separately from provider TTFT.
- A CI non-inferiority check against the last accepted Light baseline on every
  change to the handler, canonical types, router, provider codecs, admission,
  telemetry, or streaming path. Run the external Bifrost comparison on release
  candidates and scheduled performance infrastructure.

## Rollout Plan

1. Check in the benchmark harness, mock provider, payload corpus, benchmark
   manifest, and direct-provider baseline. Pin Bifrost and agentgateway
   references and record the first capacity curves before implementing the
   Light path; Bifrost remains the release non-inferiority comparator.
2. Freeze the MVP application-body contract and audit durability profiles.
   Build focused prototypes for one-pass body capture/security ordering and the
   single-writer WAL/group-commit watermark. Measure body copies, handler locks,
   `fdatasync`, batch size, and commit wait before selecting implementation
   defaults. No provider request is part of this prototype.
3. Add canonical inference types, typed errors, cancellation, and streaming to
   `model-provider`, preserving an adapter for existing `light-agent` and
   `light-workflow` callers.
4. Build provider conformance tests and enable a small server-safe provider set.
   Start with at least two different provider formats so cross-provider
   normalization and fallback are exercised rather than assumed.
5. Add `crates/llm-gateway` with a checked-in local `llm-router.yml` projection,
   immutable root, alias resolution, request validation, ordered fallback,
   usage normalization, and protocol-neutral tests. This local projection is a
   vertical-slice fixture, not a second control-plane authority.
6. Add `LlmHttpIntegration`, register the Pingora `llm` branch, and implement
   `/v1/models` plus buffered Chat Completions. Prove that body-dependent
   endpoint authorization and LLM alias policy both execute before dispatch and
   that existing MCP/WebSocket paths are unchanged.
7. Run the first 500-RPS direct/Light/Bifrost comparison on the buffered
   vertical slice. Profile full-handler-chain locks, body copies, allocations,
   and provider dispatch. Benchmark sealed-enum and preconstructed dynamic
   dispatch here; refactor the shared handler bundle only when measurements
   show it is needed.
8. Add the Portal model catalog, host registration, deployment, public alias,
   alias-route, provider-account/quota group, credential lifecycle, pricing,
   and model-policy aggregates and projections against the now-exercised
   data-plane contract.
9. Publish Portal deltas into the validated, structurally shared gateway root,
   replace the vertical-slice fixture as production authority, and migrate
   agent definitions toward alias/policy references while retaining bounded
   compatibility for existing fields.
10. Add metadata-only logical-request/physical-attempt events, envelope
    reservation, `bounded-async` delivery, the versioned local WAL,
    `local-durable` group commit/recovery, and idempotent batched delivery to the
    dedicated audit store.
11. Repeat the 500-RPS production-handler comparison with `bounded-async` audit
    enabled and publish the separate `local-durable` commit-wait/capacity
    profile. Neither profile may dispatch when its declared admission or
    durability contract is unavailable.
12. Add SSE streaming and prove bounded buffering, cancellation,
    no-post-output-fallback rules, slow-consumer deadlines/permit recovery, and
    the streaming performance gate.
13. Pass the 5,000-RPS, production-handler, streaming, and overload profiles
    before the MVP is declared production-ready.
14. Add shared token/cost budgets, richer routing, guardrails, and exact
    caching.
15. Add request-scoped LLM PII tokenization/recovery and tokenized-content
    capture. Measure placeholder preservation per model and default unresolved
    tokens to `leave-masked`. Add a durable regional `PiiVault` implementation
    only for session, asynchronous, and failover profiles, and prove its common
    security/recovery contract and separate performance SLO.
16. Add governed encrypted-raw capture and curated evaluation/training dataset
    export after access, retention, deletion, and key-isolation reviews pass.
17. Add `/v1/responses` without translating it through the less expressive Chat
    Completions representation.
18. Expand operations and provider-native compatibility only from demonstrated
    client requirements and conformance coverage.
19. Add semantic caching only with separate embedding/vector admission,
    deadline and cost slices, stampede control, and named performance profiles.

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
- Portal host registration and model policy determine the visible aliases;
  agents cannot submit a provider URL, credential, or unregistered physical
  model to bypass them.
- Request, token, output, timeout, concurrency, and attempt bounds fail closed.
- Credential rotation preserves provider-account/quota-group limits and cannot
  be used to manufacture capacity. Any enabled opaque compatibility route has
  enforceable request/byte/duration/concurrency and financial envelopes.
- Usage is recorded for success, failure, timeout, and cancellation with an
  explicit completeness/evidence state.
- A bad `llm-router.yml` reload leaves the last valid runtime active.
- Pricing-only and partial routing updates reuse unchanged snapshot subgraphs,
  publish one generation-consistent root, and remain within configured retired
  generation/memory bounds.
- Every provider dispatch is represented by one logical request and its ordered
  physical attempts in the dedicated audit sink. Required audit fails before
  dispatch when its complete envelope cannot be reserved.
- `local-durable` aliases never dispatch an attempt before its start event
  reaches the WAL durable watermark; recovery reports a durable start without
  a terminal event as incomplete and sink replay is idempotent.
- The metadata-only MVP stores no prompt, completion, tool argument, or
  reversible PII in Portal, the audit metadata tables, metrics, or traces.
- Existing MCP and WebSocket routes continue to work through their current
  handler chains.
- The versioned release benchmark meets the absolute 500-RPS and 5,000-RPS
  gates and demonstrates throughput and P50/P95/P99 gateway-added latency no
  worse than the pinned Bifrost build under identical profiles.
- Overload tests show bounded memory and queue wait, prompt `429`/`503` load
  shedding, and recovery without a residual latency backlog.
- Slow or trickle-reading stream clients hit bounded write-progress and
  absolute-lifetime deadlines without leaking upstream work or permits.
- Allocation and trace assertions prove that the steady-state path reuses
  provider clients, captures one immutable published root, performs no
  synchronous control-plane I/O, and creates no work for disabled hooks.
- Request-path assertions prove there is no control-plane lock acquisition,
  policy-map merge, provider string lookup, or parse-failure fallback to an
  ungoverned detect/opaque path.

## References

- [Bifrost repository](https://github.com/maximhq/bifrost)
- [Bifrost versus LiteLLM benchmark](https://www.getmaxim.ai/bifrost/resources/benchmarks)
- [Bifrost benchmark methodology](https://docs.getbifrost.ai/benchmarking/getting-started)
- [Bifrost reproducible benchmark tooling](https://github.com/maximhq/bifrost-benchmarking)
- [Bifrost overview](https://docs.getbifrost.ai/overview)
- [Bifrost drop-in replacement](https://docs.getbifrost.ai/features/drop-in-replacement)
- [Bifrost retries and fallbacks](https://docs.getbifrost.ai/features/retries-and-fallbacks)
- [Bifrost governance routing](https://docs.getbifrost.ai/features/governance/routing)
- [LiteLLM repository](https://github.com/BerriAI/litellm)
- [LiteLLM documentation](https://docs.litellm.ai/)
- [LiteLLM benchmark methodology](https://docs.litellm.ai/docs/benchmarks)
- [agentgateway repository](https://github.com/agentgateway/agentgateway)
- [agentgateway configuration architecture](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/architecture/configuration.md)
- [agentgateway LLM compatibility parsing](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/llm/README.md)
- [agentgateway LLM route and format types](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/llm/src/lib.rs)
- [agentgateway provider dispatch and streaming translation](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/llm/mod.rs)
- [agentgateway model and virtual-model router](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/llm/model_router.rs)
- [agentgateway HTTP and LLM request path](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/proxy/httpproxy.rs)
- [agentgateway streaming guardrail state machine](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/llm/policy/streaming_guardrails.rs)
- [agentgateway pricing catalog snapshots](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/llm/cost/mod.rs)
- [agentgateway runtime stores](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/store/mod.rs)
- [agentgateway request-time policy merging](https://github.com/agentgateway/agentgateway/blob/857281d108d5444b92ed66f5e4733225a4990426/crates/agentgateway/src/store/binds.rs)
- [OpenAI API reference](https://platform.openai.com/docs/api-reference)
- [OpenAI Responses streaming events](https://platform.openai.com/docs/api-reference/responses-streaming)
- [PostgreSQL table partitioning](https://www.postgresql.org/docs/current/ddl-partitioning.html)
- [PostgreSQL row security policies](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)
- [SQLite write-ahead logging](https://www.sqlite.org/wal.html)
