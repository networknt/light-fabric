# LLM Gateway API Contract

## Status

- Status: Proposed target contract
- Date: 2026-08-04
- Scope: public inference APIs, provider adapters, and authentication boundaries

This document defines the stable HTTP contract that agents and applications use
to call `llm-gateway`. It also defines how the gateway selects a provider wire
protocol and obtains the provider credential after policy and routing have
selected a deployment.

This is a target contract, not a claim that every endpoint is implemented. The
current Rust implementation supports `GET /v1/models` and
`POST /v1/chat/completions`, including the existing OpenAI and Anthropic
outbound codecs. The endpoint tables below label all additional surfaces as
planned.

The terms **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## Decisions

1. The preferred agent API is the OpenAI Responses-compatible
   `POST /v1/responses` endpoint. It is the contract used by Codex and other
   agents that need typed input/output items, tool calls, and event streaming.
2. `POST /v1/chat/completions` remains the broad application compatibility
   API. Existing OpenAI-compatible clients continue to work.
3. Claude and Gemini receive provider-native compatibility facades under
   namespaced base URLs. This supports their official clients without making
   the selected upstream provider part of the public model name.
4. Every request names a governed public alias. A client never supplies a
   provider URL, physical model ID, route ID, or provider credential.
5. Client protocol, canonical operation, provider protocol, and provider
   authentication are separate types. The selected provider never determines
   the response contract owed to the client.
6. Client authentication and provider authentication are separate trust
   boundaries. An inbound Light credential MUST NOT be forwarded upstream. A
   provider-delegated user credential MAY be forwarded only by an explicitly
   typed, owner-scoped delegated route to that credential's provider; it is
   never a Light credential and is never eligible for cross-provider fallback.
7. Shared multi-user production routes use provider API or workload
   credentials. A personal deployment MAY define owner-scoped native session
   connectors where the provider supports that use. The connector is visible
   only to its owner and the owner's agents and is not eligible for a common
   multi-user route pool.
8. The minimum generally available application surface is model listing, Chat
   Completions, and embeddings. Responses is an additional first-class agent
   surface, not a replacement that delays those three application endpoints.

These decisions extend, but do not weaken, the accepted
[public compatibility ADR](../../adr/llm-gateway/0001-public-compatibility.md).
OpenAI Chat Completions remains the first implemented compatibility surface;
this document defines the additive target contract.

## Goals

- Give agents and applications stable APIs that do not change when routing
  moves between OpenAI, Anthropic, xAI, Google, or a local provider.
- Support Codex, Claude Code, OpenAI-compatible SDKs, and Gemini-compatible
  clients using their natural wire formats.
- Preserve tools, structured content, reasoning metadata, usage, cancellation,
  and streaming semantics when both client and selected provider support them.
- Make unsupported conversion explicit and actionable instead of silently
  dropping fields.
- Keep provider keys, OAuth refresh material, workload credentials, and
  physical model names inside the gateway deployment boundary.

## Non-goals

- The inference API is not a public control-plane mutation API. Alias,
  deployment, pricing, credential-reference, and routing changes remain
  event-sourced Light Portal operations.
- The gateway does not execute client-side tool calls. It returns tool calls to
  the agent, which may execute them through the MCP gateway and submit results
  in a later model request.
- The initial contract does not promise lossless conversion of every
  provider-specific feature.
- Consumer subscription tokens and CLI credential caches are outside the
  gateway boundary. Personal workflow automation invokes the provider's CLI
  directly; gateway routes use API or workload credentials only.

## Architectural Model

```text
agent or application
        |
        | client protocol + Light credential
        v
client adapter -> canonical operation -> policy and alias router
                                             |
                                             v
                                  provider adapter + auth provider
                                             |
                                             | provider protocol + provider credential
                                             v
                                      provider model API
```

The implementation MUST model these dimensions independently:

| Dimension | Purpose | Initial values |
|-----------|---------|----------------|
| `ClientProtocol` | Request, response, stream, and error contract owed to the caller | `openai_responses`, `openai_chat`, `anthropic_messages`, `gemini_interactions`, `gemini_generate_content` |
| `Operation` | Provider-neutral intent used by policy and capability checks | `generate`, `embed`, `rerank`, `count_tokens`, `list_models`, `get_result`, `cancel_result`, `delete_result` |
| `ProviderProtocol` | Wire contract used for the selected upstream | `openai_responses`, `openai_chat`, `anthropic_messages`, `xai_responses`, `xai_chat`, `gemini_interactions`, `gemini_generate_content`, `vertex_generate_content` |
| `ProviderProfileType` | Credential, transport, and eligibility class of the route | `openai`, `anthropic`, `xai`, `google_gemini`, `google_vertex` |
| `ProviderAuthMode` | How upstream authorization headers are produced | `bearer_secret`, `x_api_key_secret`, `google_api_key_secret`, `oauth2_workload`, `google_adc` |

The canonical representation MUST retain typed text, image and document input,
tool definitions and calls, tool results, structured-output constraints, usage,
finish status, safety results, and provider extensions that policy explicitly
allows. A conversion MUST fail before dispatch when a required feature cannot
be represented by the selected provider protocol.

## Public Base URLs

Use separate base URLs for native compatibility. This avoids the ambiguous
`GET /v1/models` response shapes used by multiple providers.

| Client | Configured base URL | Example effective endpoint |
|--------|---------------------|----------------------------|
| Codex and OpenAI-compatible agents/apps | `https://gateway.example/v1` | `POST /v1/responses` |
| Claude Code and Anthropic SDKs | `https://gateway.example/anthropic` | `POST /anthropic/v1/messages` |
| Google Gen AI SDK and Gemini REST clients | `https://gateway.example/gemini` | `POST /gemini/v1beta/models/{alias}:generateContent` |

The namespaced paths are public client compatibility surfaces. They do not
select an Anthropic or Google upstream. For example, an Anthropic Messages
request MAY route to a Google model if the selected alias declares a
conformant Messages conversion.

### Alias policy

The public `model` value MUST be a governed virtual alias such as
`coding-default`, `fast-chat`, or `embedding-default`. Provider-prefixed names
such as `openai/gpt-4o`, `anthropic/claude-sonnet`, or
`google/gemini-pro` are deliberately not a second routing mechanism.

Provider-prefixed model names are convenient in a developer proxy, but in
Light they would expose physical-provider choice, couple applications to a
deployment, and let clients bypass alias policy and approved fallback groups.
An administrator MAY create an alias whose display name contains a provider
word for migration compatibility, but it is still an ordinary governed alias;
the prefix has no routing semantics.

## Endpoint Contract

### Canonical OpenAI-compatible surface

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `GET /v1/models` | Implemented | `list_models` | Return only authorized public aliases in OpenAI model-list format. |
| `GET /v1/models/{alias}` | Planned | `list_models` | Return one authorized public alias or an indistinguishable not-found result. |
| `POST /v1/responses` | Planned, preferred for agents | `generate` | OpenAI Responses-compatible buffered or SSE generation, including typed items and tool calls. |
| `GET /v1/responses/{response_id}` | Planned | `get_result` | Retrieve a stored or background response only when the alias and route support retained results. |
| `DELETE /v1/responses/{response_id}` | Planned | `delete_result` | Delete gateway-owned retained response state and request provider deletion where applicable. |
| `POST /v1/chat/completions` | Implemented | `generate` | OpenAI Chat Completions-compatible buffered or SSE generation. |
| `POST /v1/embeddings` | Planned | `embed` | OpenAI-compatible embedding request and response. |
| `POST /v1/rerank` | Planned, extended | `rerank` | Cohere/Jina-style reranking after a canonical rerank operation and pricing contract exist. |

`POST /v1/responses` is the standard agent contract. It MUST support, subject
to alias capabilities:

- string and typed item input;
- system or developer instructions;
- client-side function tools and tool results;
- structured text output;
- reasoning controls and summaries where representable;
- `previous_response_id` only when retained state is enabled for the alias;
- buffered JSON and OpenAI Responses SSE events;
- client cancellation propagated to the active upstream request.

The first release of Responses support MAY require `store: false`. If retained
responses are not enabled, `store: true`, `previous_response_id`, retrieval,
and deletion MUST return `unsupported_feature`; they MUST NOT be silently
ignored.

### Anthropic-compatible surface

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `POST /anthropic/v1/messages` | Planned | `generate` | Anthropic Messages-compatible buffered or SSE generation. |
| `POST /anthropic/v1/messages/count_tokens` | Planned | `count_tokens` | Count the canonical request using the resolved alias/model tokenizer. |
| `GET /anthropic/v1/models` | Planned | `list_models` | Return authorized public aliases in Anthropic model-list format. |
| `GET /anthropic/v1/models/{alias}` | Planned | `list_models` | Return one authorized alias in Anthropic model format. |

The gateway SHOULD support the headers and streaming events required by current
Claude Code gateway clients. `anthropic-version` MUST be validated against an
explicit supported-version list. `anthropic-beta` capabilities MUST be
allowlisted per alias and MUST NOT be copied upstream blindly.

Claude Code is configured with an Anthropic-format base URL, for example:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example/anthropic
export ANTHROPIC_AUTH_TOKEN="$LIGHT_LLM_TOKEN"
```

`ANTHROPIC_AUTH_TOKEN` is a Light-issued gateway credential in this setup. It
is not an Anthropic API key. Claude Code sends it as an authorization header;
the gateway authenticates the developer, removes the inbound credential, and
later obtains the selected route's upstream credential.

### Gemini-compatible surface

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `POST /gemini/v1beta/interactions` | Planned, preferred Gemini agent facade | `generate` | Create a Gemini Interactions-compatible agent request; buffered, streamed, or background according to declared capabilities. |
| `GET /gemini/v1beta/interactions/{id}` | Planned | `get_result` | Retrieve or resume a retained interaction. |
| `POST /gemini/v1beta/interactions/{id}/cancel` | Planned | `cancel_result` | Cancel a background interaction. |
| `DELETE /gemini/v1beta/interactions/{id}` | Planned | `delete_result` | Delete retained interaction state. |
| `POST /gemini/v1beta/models/{alias}:generateContent` | Planned | `generate` | Gemini GenerateContent-compatible buffered generation. |
| `POST /gemini/v1beta/models/{alias}:streamGenerateContent` | Planned | `generate` | Gemini GenerateContent-compatible SSE generation. |
| `POST /gemini/v1beta/models/{alias}:embedContent` | Planned | `embed` | Generate one embedding in Gemini format. |
| `POST /gemini/v1beta/models/{alias}:batchEmbedContents` | Planned | `embed` | Generate multiple embeddings in Gemini format. |
| `POST /gemini/v1beta/models/{alias}:countTokens` | Planned | `count_tokens` | Count tokens for a Gemini-format request. |
| `GET /gemini/v1beta/models` | Planned | `list_models` | Return authorized public aliases in Gemini model-list format. |

The `{alias}` path component is always a public alias even though the native
Gemini API calls that component a model. The gateway MUST reject `models/`
resource names, provider project paths, and physical model identifiers that do
not resolve to an authorized alias.

Gemini Interactions is included because Google now recommends it as the agentic
primitive. It is not used as the gateway's internal canonical model. The
gateway still translates it into typed canonical operations before routing.

### Deferred surfaces

The following APIs require separate capability and storage designs and are not
part of the first multi-provider contract:

- `POST /v1/images/generations` and other image/video generation APIs;
- `POST /v1/audio/transcriptions`, `POST /v1/audio/speech`, and realtime
  speech APIs;
- provider-hosted files, vector stores, caches, and prompt resources;
- asynchronous batch inference;
- provider-hosted managed agents, sandboxes, skills, or environments;
- provider-specific search, code execution, and hosted MCP tools.

They MAY be added later as typed operations. They MUST NOT be exposed through
opaque pass-through routes that bypass Light authorization, policy, accounting,
or audit controls.

`POST /v1/rerank` is ahead of media APIs in the roadmap because it has a small,
bounded request/response contract and is directly useful to RAG applications.
It still requires provider-neutral documents, scores, token/cost accounting,
and an alias capability before it can be enabled.

## Operational Endpoints

Operational endpoints are not inference endpoints and do not use a model alias.
They SHOULD be exposed only on an internal listener or protected management
network.

| Method and path | Status | Contract |
|-----------------|--------|----------|
| `GET /health` | Implemented by `light-gateway` | Process liveness only; it does not promise that an LLM route is eligible. |
| `GET /readyz` | Planned | Readiness for accepting traffic, including a valid published snapshot; it MUST NOT fail merely because one optional provider is unhealthy. |
| `GET /metrics` | Planned Prometheus compatibility | Bounded-cardinality request, stream, latency, usage, cost, route-health, and error metrics. No prompts, outputs, aliases with unbounded user input, or credential data. |

The existing Light metrics handler and durable LLM audit pipeline remain the
authoritative integration points. A Prometheus endpoint is an additional
scrape format, not a replacement for accounting or durable audit delivery.

There is intentionally no public `POST /v1/gateway/keys`. Gateway client keys,
aliases, deployments, budgets, and access policy are control-plane aggregates.
They MUST be created through authorized event-sourced Light Portal commands so
that projections, snapshot export, replay, and audit history stay consistent.

## Request Rules

### Model alias

- OpenAI, Anthropic, and Responses requests use the `model` body field.
- Gemini GenerateContent and embedding requests use `{alias}` in the path.
- Gemini Interactions requests use the `model` field when the interaction is
  model-backed; managed `agent` resources are deferred.
- The alias is resolved against the request's host, environment, subject,
  operation, and current immutable routing snapshot.
- Responses MUST echo the requested public alias, not the physical provider
  model name, unless a protocol explicitly requires a distinct field. Physical
  names remain internal telemetry with restricted access.

### Streaming

The gateway owes the caller the selected client protocol's stream:

- Responses: named SSE events such as `response.output_text.delta` and a
  terminal response event;
- Chat Completions: `data:` chunks ending in `[DONE]`;
- Anthropic Messages: Anthropic message/content block SSE events;
- Gemini GenerateContent: Gemini SSE response objects;
- Gemini Interactions: Gemini interaction events with resumable event IDs when
  retained state is enabled.

Provider events are decoded and re-encoded; they are not copied as arbitrary
bytes across different protocols. After semantic output begins, the gateway
MUST NOT retry or fail over to another provider. Cancellation and disconnect
MUST propagate upstream.

### Headers

- `Authorization: Bearer <Light credential>` is the canonical inbound
  authentication form.
- The Anthropic facade MAY accept `x-api-key` for SDK compatibility, but the
  value is a Light-issued credential, not a provider key.
- The Gemini facade MAY accept `x-goog-api-key` for SDK compatibility, but the
  value is a Light-issued credential, not a Google provider key.
- `traceparent`, `tracestate`, and the Light correlation header MAY be accepted
  according to the common handler chain.
- Provider-specific beta, organization, project, account, and routing headers
  MUST NOT be forwarded unless a typed, per-capability allowlist permits them.
- All inbound Light credential headers MUST be stripped before provider
  dispatch. Raw inbound headers are never copied generically.

The gateway returns `x-request-id` on every response and SHOULD also return the
client protocol's conventional request ID header where it differs.

## Error Contract

Internally, every failure maps to a stable `GatewayError` category. The client
adapter renders that category in the caller's native error envelope.

| Internal code | Typical HTTP status | Meaning |
|---------------|---------------------|---------|
| `invalid_request` | 400 | The request does not conform to the selected client protocol. |
| `unknown_alias` | 404 | No authorized alias is visible to the caller. |
| `unsupported_feature` | 400 | The alias or selected conversion cannot preserve a requested feature. |
| `authentication_failed` | 401 | The Light client credential is absent or invalid. |
| `access_denied` | 403 | The authenticated subject cannot invoke the alias/operation. |
| `budget_exceeded` | 429 | A request, token, cost, or organizational budget rejected admission. |
| `no_eligible_route` | 503 | No active, priced, credentialed, healthy route can serve the operation. |
| `provider_auth_failed` | 502 | The selected upstream credential was rejected. Operators receive the route-safe diagnostic. |
| `provider_rate_limited` | 429 or 503 | The selected upstream quota is exhausted; retry metadata is sanitized. |
| `provider_unavailable` | 502 or 503 | The upstream failed before semantic output began. |
| `deadline_exceeded` | 504 | The request exceeded its effective deadline. |
| `stream_interrupted` | protocol terminal event | Upstream failed after semantic output began. |

Errors MUST include the request ID and an actionable, sanitized message. They
MUST NOT contain provider credentials, raw credential references, private
provider response bodies, or physical route details. A bare
`GENERIC_EXCEPTION` or “failed without an error response” is not a conformant
public error.

## Provider Adapter Contract

A provider adapter is selected only after alias authorization and route
eligibility have succeeded. It owns:

- canonical request validation for its protocol;
- conversion to the physical provider request;
- provider authentication headers;
- buffered and streaming response decoding;
- usage and finish-state normalization;
- typed provider error classification;
- cancellation and deadline propagation;
- a declared capability set used before dispatch.

The adapter MUST NOT read a client-supplied provider name, URL, or provider
credential. The provider base URL must be
validated control-plane configuration and must pass the existing SSRF and
authority controls.

### Supported provider profiles

| Provider profile | Provider protocol | Default upstream base | Production authentication | Notes |
|------------------|-------------------|-----------------------|---------------------------|-------|
| `openai` | `openai_responses`, with `openai_chat` compatibility | `https://api.openai.com/v1` | `Authorization: Bearer` from an OpenAI Platform API-key secret reference | Shared or owner-scoped server-to-server route using Platform API billing. |
| `anthropic` | `anthropic_messages` | `https://api.anthropic.com` | `x-api-key` from an Anthropic Console secret reference, or short-lived bearer token from approved workload identity; fixed `anthropic-version` | Direct Claude API. Cloud-hosted Claude needs a separate Bedrock, Vertex, or other cloud adapter because IAM and wire contracts differ. |
| `xai` | `xai_responses`, with `xai_chat` compatibility | `https://api.x.ai/v1` | `Authorization: Bearer` from an xAI API-key secret reference | Grok supports Responses and Chat Completions. Prefer Responses for agent routes. |
| `google_gemini` | `gemini_interactions`, `gemini_generate_content` | `https://generativelanguage.googleapis.com` | `x-goog-api-key` from a Gemini API-key secret reference | Developer API profile. Interactions is preferred for native Gemini agents; GenerateContent remains the broad native format. |
| `google_vertex` | `vertex_generate_content` | validated regional or global Vertex AI authority | Short-lived OAuth bearer token obtained through ADC or workload identity | Production Google Cloud profile. The gateway refreshes tokens; Portal stores configuration and references, not access tokens. |

Optional mTLS is a transport property layered on the provider profile. For
example, xAI mTLS still requires its bearer API key. Certificate references
must use the same secret-materialization boundary as other provider secrets.

## Provider Authentication

### Shared production routes

Shared routes MUST use credentials intended for server-to-server API access:

- OpenAI Platform API key for OpenAI models;
- Anthropic Console API key or approved workload-identity bearer token for the
  direct Claude API;
- xAI API key for Grok;
- Gemini API key for the Gemini Developer API;
- Google ADC, service-account impersonation, or workload identity for Vertex
  AI.

Static values are loaded only through a local secret reference such as
`env:OPENAI_API_KEY`; they are never published in the control-plane snapshot.
Refreshable auth modes produce request headers at dispatch time and refresh
before expiry without changing the published route generation.

### Personal CLI automation boundary

Codex, Claude Code, Gemini CLI, and similar tools may authenticate with a
personal subscription. Those sessions represent an individual product
entitlement and are not provider API credentials. Light Gateway MUST NOT load,
store, delegate, or proxy those sessions.

A personal workflow may invoke each supported CLI directly in its documented
non-interactive or structured-output mode. The workflow owns process
isolation, prompt and result conversion, tool execution, and retrying a task
with another CLI. Such a retry is a workflow decision, not gateway route
fallback, because it changes the agent runtime and subscription principal.

The same workflow may call Light Gateway when it wants API-backed routing.
Those routes use configured API keys or workload credentials and may fail over
between providers only under the normal capability, policy, accounting, and
pre-output fallback rules.

## Configuration Model

The following YAML is illustrative target configuration. The event-sourced
Portal model remains authoritative; projection rows MUST be produced from
events and secret values remain local to the gateway instance.

```yaml
providerProfiles:
  openai-primary:
    providerType: openai
    protocol: openai_responses
    baseUrl: https://api.openai.com/v1
    scope: shared
    auth:
      mode: bearer_secret
      secretRef: env:OPENAI_API_KEY

  anthropic-primary:
    providerType: anthropic
    protocol: anthropic_messages
    baseUrl: https://api.anthropic.com
    auth:
      mode: x_api_key_secret
      secretRef: env:ANTHROPIC_API_KEY
    headers:
      anthropic-version: "2023-06-01"

  xai-primary:
    providerType: xai
    protocol: xai_responses
    baseUrl: https://api.x.ai/v1
    auth:
      mode: bearer_secret
      secretRef: env:XAI_API_KEY

  gemini-developer:
    providerType: google_gemini
    protocol: gemini_generate_content
    baseUrl: https://generativelanguage.googleapis.com
    auth:
      mode: google_api_key_secret
      secretRef: env:GEMINI_API_KEY

  gemini-vertex:
    providerType: google_vertex
    protocol: vertex_generate_content
    baseUrl: https://aiplatform.googleapis.com
    project: example-project
    location: global
    auth:
      mode: google_adc
      scopes:
        - https://www.googleapis.com/auth/cloud-platform

```

A deployment binds one provider profile to a physical model and declared
capabilities. A public alias binds policy and pricing to one or more eligible
deployments. API clients see only the alias. The persisted control-plane shape
routes through deployment aggregates rather than directly from an alias to a
provider profile.

## Agent and CLI Profiles

### Codex CLI

Codex can use the gateway as a custom Responses provider. The gateway token is
supplied through a dedicated environment variable or a command-backed token
helper, not through the user's OpenAI provider key.

```toml
model = "coding-default"
model_provider = "light_gateway"

[model_providers.light_gateway]
name = "Light LLM Gateway"
base_url = "https://gateway.example/v1"
wire_api = "responses"
env_key = "LIGHT_LLM_TOKEN"
```

Codex subscription authentication is not forwarded through this profile. A
workflow that wants to use the personal Codex subscription invokes Codex CLI
directly; a Codex CLI configured as a Light Gateway client uses the Light
credential above and consumes an API-backed gateway route.

### Claude Code

Claude Code uses the Anthropic facade shown earlier. The gateway must keep pace
with Claude Code's documented gateway protocol and explicitly allow newly
required beta headers and message fields before advertising compatibility.
Pointing Claude Code at a gateway credential replaces subscription billing for
that session; the selected upstream account is billed.

### Grok applications

Grok applications use the canonical OpenAI-compatible base URL and select a
public alias routed to an xAI deployment. No Grok-specific client path is
needed because xAI supports Responses and Chat Completions. The client receives
OpenAI-compatible output while the provider adapter authenticates to xAI with
the route's `XAI_API_KEY` reference.

### Gemini applications

Gemini-native clients use the `/gemini` base URL and a Light-issued credential.
OpenAI-compatible applications can use `/v1/responses` or
`/v1/chat/completions` with the same Gemini-backed alias. Vertex AI is an
upstream deployment profile, not a different public client API.

## Capability and Conversion Rules

Every deployment publishes a verified capability set. Route eligibility is the
intersection of alias policy, requested client features, canonical operation,
provider capabilities, credential readiness, price readiness, health, and
environment.

At minimum, generation capabilities distinguish:

- buffered and streaming output;
- text, image, audio, document, and video input;
- client-side function tools and parallel tool calls;
- structured JSON output;
- reasoning controls and summaries;
- retained response/interaction state;
- prompt caching controls;
- safety configuration and safety-result visibility;
- exact usage and provider cost reporting.

Unknown client fields may be preserved only for bounded same-format forwarding
under an explicit compatibility allowlist. Cross-format conversion uses typed
canonical fields. Required or behavior-changing fields that cannot be mapped
cause `unsupported_feature` before provider dispatch.

## Rust Implementation Alignment

The generic recommendation to start with Axum is sound for a new standalone
service, but `light-gateway` is not a greenfield Axum application. It already
uses Pingora listeners, the ordered Light handler chain, shared correlation and
security handlers, and a compiled LLM runtime. The API work MUST extend that
path instead of introducing a second HTTP server or middleware stack.

- Reuse preconstructed provider clients and connection pools from the compiled
  runtime snapshot; do not construct an HTTP client per request.
- Represent buffered and streaming results with async streams and typed codec
  events. Provider SSE is decoded incrementally and encoded into the client
  protocol without buffering the entire completion.
- Use typed `serde` request models. Unknown fields are not globally lenient:
  they may enter only the existing bounded compatibility envelope for approved
  same-format forwarding. A malformed known field is a terminal parse error.
- Normalize provider usage into canonical input, output, cached, reasoning, and
  total token fields before rendering OpenAI `prompt_tokens`/
  `completion_tokens`, Anthropic `input_tokens`/`output_tokens`, or Gemini
  usage metadata.
- Preserve the existing handler-chain order so authentication, authorization,
  admission limits, policy, accounting, audit, and provider dispatch cannot be
  bypassed by a new compatibility path.

## Delivery Plan

1. **Contract foundation:** generalize `ClientFormat`, `Operation`,
   `ProviderFormat`, capability validation, and provider auth without changing
   the existing Chat Completions behavior.
2. **Responses and Codex:** add `POST /v1/responses`, Responses SSE, OpenAI and
   xAI Responses adapters, and a Codex CLI smoke test.
3. **Claude facade:** add namespaced Messages, token counting, Anthropic SSE,
   and Claude Code conformance fixtures.
4. **Gemini facade:** add GenerateContent, streaming, embeddings, token
   counting, and Gemini Developer API/Vertex auth profiles.
5. **Agent state:** add Responses retrieval/deletion and Gemini Interactions
   only after retention ownership, route affinity, deletion, and audit rules
   are implemented.
6. **Workflow integration boundary:** document and test that personal CLI
   sessions remain in workflow-owned adapters while Light Gateway provider
   profiles accept API keys or workload credentials only.

## Acceptance Criteria

- Official Codex CLI can complete a tool-calling turn through `/v1/responses`
  using a Light-issued bearer credential.
- Provider configuration rejects personal subscription sessions, CLI
  credential caches, and delegated consumer credentials as provider auth.
- Official Claude Code can complete buffered, streaming, tool-use, and token
  counting flows through `/anthropic/v1` using a Light-issued credential.
- Official OpenAI SDKs can call OpenAI-, Anthropic-, xAI-, and Gemini-backed
  aliases without seeing a physical provider model.
- Official Google Gen AI SDK fixtures that support a custom base URL can call
  the `/gemini/v1beta` facade with explicit Light authentication headers.
- Inbound gateway credentials are proven absent from all recorded upstream
  requests; provider credentials are proven absent from logs, errors, audit
  payloads, and client responses.
- Representative accepted and rejected payloads are parsed and validated for
  each client/provider pair; tests assert semantic output, errors, streaming
  order, tool-call identity, usage, and cancellation rather than text fixtures
  alone.
- A requested feature that cannot survive conversion fails before dispatch
  with `unsupported_feature` and an actionable message.
- A route is ineligible when credential, pricing, capability, environment, or
  health data is missing, with `no_eligible_route` explaining the missing
  category without revealing secrets.
- Existing Chat Completions and model-list qualification gates remain green.

## Provider References

- [OpenAI Responses API](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Codex custom model providers](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers)
- [Codex authentication](https://learn.chatgpt.com/docs/auth)
- [Claude API overview and authentication](https://platform.claude.com/docs/en/api/overview)
- [Claude Code gateway guidance](https://code.claude.com/docs/en/llm-gateway)
- [xAI inference API](https://docs.x.ai/developers/rest-api-reference/inference/chat)
- [xAI API-key authorization](https://docs.x.ai/developers/rest-api-reference/management/auth)
- [Gemini API reference](https://ai.google.dev/api)
- [Gemini Interactions API](https://ai.google.dev/api/interactions-api)
- [Google Gen AI SDK custom base URL](https://googleapis.github.io/python-genai/#custom-base-url)
- [Vertex AI Gemini quickstart and ADC](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/start/quickstart)
