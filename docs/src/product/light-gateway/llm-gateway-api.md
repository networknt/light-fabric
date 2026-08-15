# LLM Gateway API Contract

## Status

- Status: Core API implemented; live production qualification pending
- Date: 2026-08-14
- Scope: public inference APIs, provider adapters, and authentication boundaries

This document defines the stable HTTP contract that agents and applications use
to call `llm-gateway`. It also defines how the gateway selects a provider wire
protocol and obtains the provider credential after policy and routing have
selected a deployment.

The current Rust implementation supports model listing and retrieval, Chat
Completions, buffered and streamed Responses, and buffered embeddings. Live SDK,
Codex, provider, multi-replica publication, performance, canary, and rollback
qualification remains required before production promotion. The endpoint tables
below distinguish the implemented core from optional and deferred profiles.

The terms **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## Decisions

1. The preferred agent API is the OpenAI Responses-compatible
   `POST /v1/responses` endpoint. It is the contract used by Codex and other
   agents that need typed input/output items, tool calls, and event streaming.
2. `POST /v1/chat/completions` remains the broad application compatibility
   API. Existing OpenAI-compatible clients continue to work.
3. The required public contract is the OpenAI-compatible API family: model
   listing, Chat Completions, Responses, and embeddings. It is the stable
   provider-neutral surface for Light-controlled agents and applications.
4. Provider-native client facades are optional compatibility profiles, not
   provider-routing mechanisms. An Anthropic Messages profile is added only
   when Claude Code or another Anthropic-format client is a certified product
   requirement. A Gemini profile remains deferred until a Gemini-native client
   or feature requires it.
5. Every request names a governed public alias. A client never supplies a
   provider URL, physical model ID, route ID, or provider credential.
6. Client protocol, canonical operation, provider protocol, and provider
   authentication are separate types. The selected provider never determines
   the response contract owed to the client.
7. Client authentication and provider authentication are separate trust
   boundaries. An inbound Light credential MUST NOT be forwarded upstream. A
   provider-delegated user credential MAY be forwarded only by an explicitly
   typed, owner-scoped delegated route to that credential's provider; it is
   never a Light credential and is never eligible for cross-provider fallback.
8. Shared multi-user production routes use provider API or workload
   credentials. A personal deployment MAY define owner-scoped native session
   connectors where the provider supports that use. The connector is visible
   only to its owner and the owner's agents and is not eligible for a common
   multi-user route pool.
9. The minimum generally available application surface is model listing, Chat
   Completions, and embeddings. Responses is an additional first-class agent
   surface, not a replacement that delays those three application endpoints.
10. Portability applies only to features represented by the selected client
    contract and every eligible provider route. Unsupported or lossy
    conversion MUST fail before dispatch; the gateway does not silently drop a
    behavior-changing field to manufacture compatibility.
11. AWS Bedrock Converse is an upstream provider protocol, not a public client
    API. Core OpenAI-compatible requests and an optional Anthropic Messages
    facade normalize into the canonical representation before a Bedrock adapter
    emits Converse requests. Raw `/converse` pass-through is not exposed.

These decisions extend, but do not weaken, the accepted
[public compatibility ADR](../../adr/llm-gateway/0001-public-compatibility.md).
OpenAI Chat Completions remains the first implemented compatibility surface;
this document defines the additive target contract.

## Goals

- Give agents and applications stable APIs that do not change when routing
  moves between OpenAI, Anthropic, xAI, Google, or a local provider.
- Support OpenAI-compatible SDKs and Codex through the required core profile.
- Support off-the-shelf clients such as Claude Code through optional, explicitly
  certified compatibility profiles when product requirements justify them.
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
- The gateway is not a complete clone of every provider API. A provider-native
  client surface is not implemented merely because the corresponding upstream
  provider is supported behind the OpenAI-compatible core.
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
| `ClientProtocol` | Request, response, stream, and error contract owed to the caller | Required: `openai_responses`, `openai_chat`, `openai_embeddings`; optional profiles: `anthropic_messages`, `gemini_interactions`, `gemini_generate_content` |
| `Operation` | Provider-neutral intent used by policy and capability checks | `generate`, `embed`, `rerank`, `count_tokens`, `list_models`, `get_result`, `cancel_result`, `delete_result` |
| `ProviderProtocol` | Wire contract used for the selected upstream | `openai_responses`, `openai_chat`, `anthropic_messages`, `bedrock_converse`, `xai_responses`, `xai_chat`, `gemini_interactions`, `gemini_generate_content`, `vertex_generate_content` |
| `ProviderProfileType` | Credential, transport, and eligibility class of the route | `openai`, `anthropic`, `aws_bedrock`, `xai`, `google_gemini`, `google_vertex` |
| `ProviderAuthMode` | How upstream authorization headers are produced | `bearer_secret`, `x_api_key_secret`, `aws_bedrock_api_key`, `aws_sigv4`, `google_api_key_secret`, `oauth2_workload`, `google_adc` |

The canonical representation MUST retain typed text, image and document input,
tool definitions and calls, tool results, structured-output constraints, usage,
finish status, safety results, and provider extensions that policy explicitly
allows. A conversion MUST fail before dispatch when a required feature cannot
be represented by the selected provider protocol.

## Public Base URLs

The OpenAI-compatible base URL is the required public surface. Optional native
compatibility profiles use namespaced base URLs so their request, response,
stream, error, and model-list contracts cannot be confused with the core.

| Client | Configured base URL | Example effective endpoint |
|--------|---------------------|----------------------------|
| Codex and OpenAI-compatible agents/apps | `https://gateway.example/v1` | `POST /v1/responses` |
| Claude Code and Anthropic SDKs, when the optional profile is enabled | `https://gateway.example/anthropic` | `POST /anthropic/v1/messages` |
| Google Gen AI SDK and Gemini REST clients, when the deferred profile is enabled | `https://gateway.example/gemini` | `POST /gemini/v1beta/models/{alias}:generateContent` |

An enabled namespaced path is a client compatibility surface. It does not
select an Anthropic or Google upstream. For example, an Anthropic Messages
request MAY route to a Google model if the selected alias declares a conformant
Messages conversion. Supporting an Anthropic or Google provider behind the
core API does not require enabling the corresponding client facade.

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

The contract is divided into profiles so provider support does not imply an
unbounded public API commitment:

| Profile | Requirement | Purpose |
|---------|-------------|---------|
| `core_openai` | Required | Stable provider-neutral API for Light-controlled applications, OpenAI-compatible SDKs, and Codex. |
| `anthropic_messages` | Optional | Drop-in Claude Code and Anthropic SDK compatibility after a client conformance gate passes. |
| `gemini_native` | Deferred optional | Drop-in Google Gen AI SDK or Gemini CLI compatibility when a concrete client or native feature requires it. |
| `retained_results` | Deferred optional | Retrieval, cancellation, and deletion after state ownership and retention are designed. |
| `rerank` | Optional extended | Provider-neutral reranking for RAG applications. |

### Required OpenAI-compatible core

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `GET /v1/models` | Required core, implemented | `list_models` | Return only authorized public aliases in OpenAI model-list format. |
| `GET /v1/models/{alias}` | Required core, implemented | `list_models` | Return one authorized public alias or an indistinguishable not-found result. |
| `POST /v1/responses` | Required core, implemented; preferred for agents | `generate` | OpenAI Responses-compatible buffered or SSE generation, including typed items and tool calls. |
| `GET /v1/responses/{response_id}` | Deferred `retained_results` profile | `get_result` | Retrieve a stored or background response only when the alias and route support retained results. |
| `DELETE /v1/responses/{response_id}` | Deferred `retained_results` profile | `delete_result` | Delete gateway-owned retained response state and request provider deletion where applicable. |
| `POST /v1/chat/completions` | Required core, implemented | `generate` | OpenAI Chat Completions-compatible buffered or SSE generation. |
| `POST /v1/embeddings` | Required core, implemented | `embed` | OpenAI-compatible embedding request and response. |
| `POST /v1/rerank` | Optional extended profile | `rerank` | Cohere/Jina-style reranking after a canonical rerank operation and pricing contract exist. |

`POST /v1/responses` is the standard agent contract. It MUST support, subject
to alias capabilities:

- string and typed item input;
- system or developer instructions;
- client-side function tools and tool results;
- structured text output;
- reasoning controls and summaries where representable;
- stateless opaque reasoning continuity through the standard Responses
  reasoning item when the selected route requires it;
- `previous_response_id` only when retained state is enabled for the alias;
- buffered JSON and OpenAI Responses SSE events;
- client cancellation propagated to the active upstream request.

The first release of Responses support MAY require `store: false`. If retained
responses are not enabled, `store: true`, `previous_response_id`, retrieval,
and deletion MUST return `unsupported_feature`; they MUST NOT be silently
ignored.

Opaque reasoning continuity is client-protocol-specific and does not imply
gateway-owned conversation state. For stateless `/v1/responses`, the gateway
returns a gateway-sealed `encrypted_content` value when the upstream turn
produces opaque continuation state; it does not manufacture a reasoning blob
for a response without such state. The caller returns the complete reasoning
item as input on the subsequent turn. The legacy
`include: ["reasoning.encrypted_content"]` request remains accepted but is not
required. The gateway MUST bind the opaque state to the tenant, public alias,
client protocol, selected deployment, and that deployment's provider-client
material generation. It MUST reject tampering and cross-route replay and MUST
NOT log or render the provider continuation material as public reasoning text.

Reasoning envelopes use a gateway-wide, host/environment-scoped key set
distributed through the existing credential-reference and local secret
materialization boundary using `credentialPurpose: REASONING_SEAL`; it is not a
provider-endpoint credential. The immutable `values.yml` snapshot carries exactly one current key
ID and reference, at most one previous key ID and reference, key-set generation,
and bounded item/count/aggregate limits. It never carries key bytes. Every
serving replica MUST resolve the same key set before acknowledging readiness; a
random per-process boot key is forbidden. New envelopes use the current key.
The previous key decrypts while it remains in the active key-set generation.

Key retirement is generation-based, not time-based. A replacement generation
is prepared and resolved by every required replica before fleet promotion. The
serving set MUST NOT contain replicas on different active key-set generations.
Removing the previous key in a later promoted generation retires it for the
entire serving fleet; replica wall clocks do not participate in acceptance.

Rotating the selected provider endpoint credential changes that deployment's
provider-client material generation and deliberately invalidates its older
reasoning envelopes with `reasoning_state_stale`; rotation of an unrelated
endpoint does not. The generation is derived from the endpoint authentication
policy and active credential identity/version. Routine refresh of short-lived
SigV4 session credentials does not change it. `reasoning_state_stale` is
non-retryable for the existing conversation: the caller MUST restart from a new
initial request. Dropping the stale item and resubmitting its tool result does
not create an escape hatch; it fails with `reasoning_state_required`.

A continuity-required turn containing assistant tool-call history and a tool
result without the associated reasoning item fails before dispatch with
`reasoning_state_required`. Tampered, replayed, unknown-key, retired-key,
oversized, or over-count state likewise fails before dispatch with a stable
typed error.

A continuity-required alias MAY have multiple deployments for new
conversations. Once sealed state is returned, the subsequent request is pinned
to its bound deployment before preference ordering. If that deployment is no
longer mapped or eligible, the gateway returns `reasoning_route_unavailable`
without revealing physical identity. A request carrying sealed state is never
eligible for fallback, even when the bound provider fails before output. The
gateway MUST NOT discard reasoning state to start a fresh chain implicitly.

`/v1/chat/completions` has no portable opaque reasoning-state item. A deployment
that requires reasoning continuity, including for a multi-turn tool exchange,
is ineligible for Chat Completions and MUST fail before provider dispatch. An
optional native client protocol, such as Anthropic Messages, MAY use its own
typed signed/redacted reasoning blocks after independent qualification.

The portable first-release function-tool profile accepts `strict` when omitted
or `false`. `strict: true` is rejected as `unsupported_feature` until strict
tool-schema preservation is represented as a routed capability across every
eligible provider protocol; it is never silently weakened. The deprecated
Responses `user` forwarding field is likewise outside the stateless portable
profile because its provider-side safety semantics cannot be preserved across
routes.

For embeddings, provider responses are decoded into canonical finite `f32`
vectors. The gateway requests provider `float` encoding, validates declared
dimensions and response bounds, and renders either client `float` JSON or
gateway-re-encoded little-endian `base64`. Consequently,
`supported_encodings` gates the canonical provider-side `float` seam; client
base64 output does not require provider base64 passthrough.

### Durable embedding-space stability

OpenAI-compatible request and response bodies do not identify a vector space.
An embedding alias therefore declares an immutable `embeddingSpace` contract:
space ID and revision, dimension, normalization, distance metric, and document
input-transform version. Every eligible primary, fallback, and canary must
publish the exact same contract. Matching dimensions alone are insufficient.

Knowledge Base aliases set `requireExpectedEmbeddingSpace: true`. Their clients
send both `x-light-expected-embedding-space-id` and
`x-light-expected-embedding-space-revision`. A missing, partial, malformed, or
mismatched expectation fails before provider dispatch. Successful qualified
calls return `x-light-embedding-space-id`,
`x-light-embedding-space-revision`, and `x-light-config-generation`; ordinary
SDK calls that omit the expectation do not receive the configuration generation.
The gateway pins or injects the contract dimension on required-space aliases.

Budgeted embedding clients may also send
`x-light-maximum-billed-cost-micros`. The gateway combines that request ceiling
with the alias ceiling and rejects the request before provider dispatch when
its conservative multi-attempt reservation envelope cannot fit. Every
successful embedding response returns `x-light-billed-cost-micros` from the
gateway's reconciled pricing ledger; callers must not infer billed cost from
token counts or provider-specific response fields.

`Idempotency-Key` is currently accepted only as forward-compatible request
metadata; the gateway does not deduplicate embedding dispatch or billing by
that header. Durable ingestion workers must commit vectors with their own
input-hash/job idempotency record. Server-side dispatch deduplication remains a
future extension and must not be assumed by clients.

Query and indexing traffic use separate, network-restricted gateway instances
configured with `embeddingWorkloadLane: kb_query` and `kb_index`. A request
header cannot select a lane. Both lanes may use different budgets and capacity,
but must advertise the same embedding-space contract. OpenAI-compatible local
servers such as llama.cpp or Ollama can participate as physical embedding
deployments after endpoint, model, dimension, transform behavior, and the
operator-approved space revision pass the same conformance and drift gates.

### Optional Anthropic Messages profile

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `POST /anthropic/v1/messages` | Optional, planned only for certified clients | `generate` | Anthropic Messages-compatible buffered or SSE generation. |
| `POST /anthropic/v1/messages/count_tokens` | Optional, client-driven | `count_tokens` | Count the canonical request using the resolved alias/model tokenizer. |
| `GET /anthropic/v1/models` | Deferred compatibility convenience | `list_models` | Return authorized public aliases in Anthropic model-list format. |
| `GET /anthropic/v1/models/{alias}` | Deferred compatibility convenience | `list_models` | Return one authorized alias in Anthropic model format. |

This profile is required only when Claude Code, the Claude Agent SDK, or an
existing Anthropic-format application is explicitly certified as a supported
client. Enabling it does not constrain the selected upstream to Anthropic.
When enabled, the gateway MUST support the headers and streaming events in its
pinned client conformance profile. `anthropic-version` MUST be validated
against an explicit supported-version list. `anthropic-beta` capabilities MUST
be allowlisted per alias and MUST NOT be copied upstream blindly.

Claude Code is configured with an Anthropic-format base URL, for example:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example/anthropic
export ANTHROPIC_AUTH_TOKEN="$LIGHT_LLM_TOKEN"
```

`ANTHROPIC_AUTH_TOKEN` is a Light-issued gateway credential in this setup. It
is not an Anthropic API key. Claude Code sends it as an authorization header;
the gateway authenticates the developer, removes the inbound credential, and
later obtains the selected route's upstream credential.

### Deferred optional Gemini-native profile

| Method and path | Status | Canonical operation | Contract |
|-----------------|--------|---------------------|----------|
| `POST /gemini/v1beta/interactions` | Deferred `gemini_native` and `retained_results` profiles | `generate` | Create a Gemini Interactions-compatible agent request; buffered, streamed, or background according to declared capabilities. |
| `GET /gemini/v1beta/interactions/{id}` | Deferred `retained_results` profile | `get_result` | Retrieve or resume a retained interaction. |
| `POST /gemini/v1beta/interactions/{id}/cancel` | Deferred `retained_results` profile | `cancel_result` | Cancel a background interaction. |
| `DELETE /gemini/v1beta/interactions/{id}` | Deferred `retained_results` profile | `delete_result` | Delete retained interaction state. |
| `POST /gemini/v1beta/models/{alias}:generateContent` | Deferred `gemini_native` profile | `generate` | Gemini GenerateContent-compatible buffered generation. |
| `POST /gemini/v1beta/models/{alias}:streamGenerateContent` | Deferred `gemini_native` profile | `generate` | Gemini GenerateContent-compatible SSE generation. |
| `POST /gemini/v1beta/models/{alias}:embedContent` | Deferred `gemini_native` profile | `embed` | Generate one embedding in Gemini format. |
| `POST /gemini/v1beta/models/{alias}:batchEmbedContents` | Deferred `gemini_native` profile | `embed` | Generate multiple embeddings in Gemini format. |
| `POST /gemini/v1beta/models/{alias}:countTokens` | Deferred `gemini_native` profile | `count_tokens` | Count tokens for a Gemini-format request. |
| `GET /gemini/v1beta/models` | Deferred compatibility convenience | `list_models` | Return authorized public aliases in Gemini model-list format. |

Gemini models remain eligible upstreams for the required OpenAI-compatible
core even while this client profile is disabled. The profile is enabled only
when a Google Gen AI SDK, Gemini CLI, or native-only feature is a certified
requirement. If enabled, the `{alias}` path component is always a public alias
even though the native Gemini API calls that component a model. The gateway
MUST reject `models/` resource names, provider project paths, and physical
model identifiers that do not resolve to an authorized alias.

Gemini Interactions is not used as the gateway's internal canonical model. It
remains behind both the `gemini_native` and `retained_results` profiles because
its background and retained semantics require an explicit state design.

### Deferred surfaces

The following APIs require separate capability and storage designs and are not
part of the required OpenAI-compatible core:

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

- OpenAI Chat, Responses, and embedding requests use the `model` body field.
- An enabled Anthropic Messages profile uses the `model` body field.
- An enabled Gemini GenerateContent or embedding profile uses `{alias}` in the
  path. Gemini Interactions uses the `model` field when the interaction is
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
- Anthropic Messages, when enabled: Anthropic message/content block SSE events;
- Gemini GenerateContent, when enabled: Gemini SSE response objects;
- Gemini Interactions, when enabled: Gemini interaction events with resumable
  event IDs when retained state is enabled.

Provider events are decoded and re-encoded; they are not copied as arbitrary
bytes across different protocols. After semantic output begins, the gateway
MUST NOT retry or fail over to another provider. Cancellation and disconnect
MUST propagate upstream.

On the Anthropic Messages facade, reasoning block ordering is provider-dependent.
For Bedrock-backed streams, reasoning blocks are emitted after text because
Bedrock provides continuation state at message completion. Gateway round-trips
remain supported, but the accumulated message is not guaranteed to be directly
replayable to Anthropic's native endpoint.

Each reasoning block is emitted as a self-contained `content_block_start` /
`content_block_stop` pair after any open text block has been closed, so at most
one content block is open at a time. The gateway does not buffer text deltas to
place reasoning blocks first; a strict-order buffering mode remains a possible
future per-deployment option. This non-buffering behavior is deliberately
pinned by regression tests.

### Headers

- `Authorization: Bearer <Light credential>` is the canonical inbound
  authentication form.
- An enabled Anthropic facade MAY accept `x-api-key` for SDK compatibility, but
  the value is a Light-issued credential, not a provider key.
- An enabled Gemini facade MAY accept `x-goog-api-key` for SDK compatibility,
  but the value is a Light-issued credential, not a Google provider key.
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
| `aws_bedrock` | `bedrock_converse` | `https://bedrock-runtime.{region}.amazonaws.com` | `Authorization: Bearer` from an AWS Bedrock API-key secret reference for development/evaluation, or SigV4 from an AWS workload identity for production | The endpoint owns the AWS runtime/signing region; the deployment owns the physical model or inference-profile ID. For example, a US cross-region inference profile may use `us.anthropic.claude-sonnet-4-6`; clients still send only a Light alias. |
| `xai` | `xai_responses`, with `xai_chat` compatibility | `https://api.x.ai/v1` | `Authorization: Bearer` from an xAI API-key secret reference | Grok supports Responses and Chat Completions. Prefer Responses for agent routes. |
| `google_gemini` | `gemini_interactions`, `gemini_generate_content` | `https://generativelanguage.googleapis.com` | `x-goog-api-key` from a Gemini API-key secret reference | Developer API upstream profile. Supporting it behind the OpenAI-compatible core does not enable the optional Gemini client facade. |
| `google_vertex` | `vertex_generate_content` | validated regional or global Vertex AI authority | Short-lived OAuth bearer token obtained through ADC or workload identity | Production Google Cloud profile. The gateway refreshes tokens; Portal stores configuration and references, not access tokens. |

Optional mTLS is a transport property layered on the provider profile. For
example, xAI mTLS still requires its bearer API key. Certificate references
must use the same secret-materialization boundary as other provider secrets.

### AWS Bedrock Converse adapter

The Bedrock profile uses the Bedrock Runtime `Converse` and `ConverseStream`
operations. It is distinct from both the direct Anthropic Messages provider
protocol and the optional public Anthropic Messages facade.

- The provider endpoint owns the AWS runtime/signing region and validated
  Bedrock Runtime authority. The provider deployment owns the physical model or
  inference-profile ID. None may be supplied or overridden by the client.
- The adapter converts canonical system content, messages, tool definitions,
  tool choice, tool use, tool results, inference parameters, stop reasons, and
  usage to and from their typed Converse equivalents.
- Conversion is explicitly fallible. A required field or semantic that cannot
  be represented by Converse MUST return `unsupported_feature` before
  dispatch; it MUST NOT be silently discarded.
- `ConverseStream` events are decoded into canonical stream events and then
  encoded into the selected client protocol. Raw AWS event-stream frames are
  never exposed to clients.
- Model IDs and inference-profile IDs are account-, region-, and
  availability-dependent deployment data. They are not a static global model
  catalog and must be live-qualified for the target AWS account and region.
- The AWS Mantle-compatible `/anthropic/v1/messages` endpoint is a separate
  upstream protocol. It MUST NOT be substituted for Converse merely because a
  model appears in the AWS catalog; it requires its own provider adapter and
  live qualification before use.

For the initial `us-east-1` qualification, Claude Sonnet 4.6 through the US
inference profile is the baseline text and native tool-use target. Catalog-only
Claude 5 entries remain ineligible until the configured account can invoke
them successfully. An always-on-reasoning deployment is additionally eligible
only for client protocols that can round-trip its opaque continuation state;
OpenAI Chat is not such a protocol.

## Provider Authentication

### Shared production routes

Shared routes MUST use credentials intended for server-to-server API access:

- OpenAI Platform API key for OpenAI models;
- Anthropic Console API key or approved workload-identity bearer token for the
  direct Claude API;
- AWS Bedrock API key for development/evaluation routes, or SigV4 credentials
  from an IAM role or other approved workload identity for production Bedrock
  routes;
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
reasoningSeal:
  state: active
  keySetGeneration: 1
  current:
    keyId: reasoning-seal-2026-08
    credentialRef: env:LLM_REASONING_SEAL_KEY
  previous: null
  limits:
    maxEncodedItemBytes: 131072
    maxDecodedProviderStateBytes: 98304
    maxItemsPerRequest: 8
    maxCumulativeEncodedBytes: 262144
    maxCumulativeDecodedBytes: 196608

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

  bedrock-us-evaluation:
    providerType: aws_bedrock
    protocol: bedrock_converse
    baseUrl: https://bedrock-runtime.us-east-1.amazonaws.com
    region: us-east-1
    auth:
      mode: aws_bedrock_api_key
      secretRef: env:AWS_BEARER_TOKEN_BEDROCK

  bedrock-us-production:
    providerType: aws_bedrock
    protocol: bedrock_converse
    baseUrl: https://bedrock-runtime.us-east-1.amazonaws.com
    region: us-east-1
    auth:
      mode: aws_sigv4
      service: bedrock

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
provider profile. For Bedrock, `physicalModelId` may be a foundation-model ID
or an inference-profile ID; it belongs on the deployment, never in a client
request or global reference value that assumes universal account availability.

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

Claude Code requires the optional Anthropic Messages profile because it speaks
the Anthropic gateway protocol. Light MUST advertise Claude Code compatibility
only after the pinned client conformance gate passes. The gateway must then
keep pace with documented required headers, stream events, beta headers, and
message fields. Pointing Claude Code at a gateway credential replaces
subscription billing for that session; the selected upstream account is
billed. If Claude Code is not a committed product client, this profile remains
disabled and creates no obligation to expose Anthropic-format endpoints.

### Grok applications

Grok applications use the canonical OpenAI-compatible base URL and select a
public alias routed to an xAI deployment. No Grok-specific client path is
needed because xAI supports Responses and Chat Completions. The client receives
OpenAI-compatible output while the provider adapter authenticates to xAI with
the route's `XAI_API_KEY` reference.

### Gemini applications

Light-controlled applications use `/v1/responses`, `/v1/chat/completions`, or
`/v1/embeddings` with a Gemini-backed alias; no Gemini public client path is
needed for that routing. A Gemini-native client uses the `/gemini` base URL and
a Light-issued credential only after the optional profile is enabled and its
client conformance gate passes. Vertex AI remains an upstream deployment
profile, not a different required public client API.

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
- reasoning controls, public summaries, and client-protocol-specific opaque
  continuation state;
- retained response/interaction state;
- prompt caching controls;
- safety configuration and safety-result visibility;
- exact usage and provider cost reporting.

Unknown client fields may be preserved only for bounded same-format forwarding
under an explicit compatibility allowlist. Cross-format conversion uses typed
canonical fields. Required or behavior-changing fields that cannot be mapped
cause `unsupported_feature` before provider dispatch.

Protocol conversion SHOULD use an explicit fallible codec or Rust `TryFrom`
implementation with structured conversion errors. An infallible `From`
implementation is appropriate only where every source value has a valid,
semantically equivalent target representation.

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
- Treat Bedrock `ConverseStream` as a typed AWS event stream rather than SSE.
  Decode its content-block, tool-use, metadata, and terminal events into the
  same canonical stream consumed by the OpenAI and optional Anthropic client
  encoders.
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

1. **Contract foundation:** generalize `ClientProtocol`, `Operation`,
   `ProviderProtocol`, capability validation, and provider auth without
   changing the existing Chat Completions behavior. Keep client protocol and
   upstream provider protocol independently selectable.
2. **Required application core:** add `GET /v1/models/{alias}` and
   `POST /v1/embeddings`, with operation-specific capability, pricing,
   accounting, audit, and provider conformance gates.
3. **Responses and Codex:** add `POST /v1/responses`, Responses SSE, OpenAI and
   xAI Responses adapters, and a Codex CLI smoke test.
4. **AWS Bedrock provider:** add the `aws_bedrock` profile, API-key and SigV4
   auth providers, `Converse`/`ConverseStream` codecs, inference-profile routing,
   and buffered, streaming, usage, error, and tool-use conformance gates. Route
   the existing OpenAI-compatible core to Bedrock before adding another public
   client facade.
5. **Optional Claude profile:** only when Claude Code is a committed client,
   add namespaced Messages, required token counting, Anthropic SSE, and pinned
   Claude Code conformance fixtures. Prove that the client facade can route to
   both direct Anthropic and Bedrock without changing its public contract. Keep
   the profile disabled otherwise.
6. **Optional Gemini profile:** only when a Gemini-native client or native-only
   feature is committed, add the smallest GenerateContent, streaming,
   embedding, token-counting, and model-list surface required by its pinned
   conformance suite.
7. **Optional retained state:** add Responses retrieval/deletion and Gemini
   Interactions only after retention ownership, route affinity, deletion,
   encryption, expiry, and audit rules are implemented.
8. **Optional rerank:** add the provider-neutral rerank operation only after
   document limits, score semantics, pricing, accounting, and conformance are
   frozen.
9. **Workflow integration boundary:** document and test that personal CLI
   sessions remain in workflow-owned adapters while Light Gateway provider
   profiles accept API keys or workload credentials only.

## Acceptance Criteria

- Official Codex CLI can complete a tool-calling turn through `/v1/responses`
  using a Light-issued bearer credential.
- Provider configuration rejects personal subscription sessions, CLI
  credential caches, and delegated consumer credentials as provider auth.
- Official OpenAI SDKs can call OpenAI-, Anthropic-, xAI-, and Gemini-backed
  aliases without seeing a physical provider model.
- Official OpenAI-compatible clients can complete buffered, streaming, and
  native tool-use turns against a Claude Sonnet 4.6 alias backed by Bedrock
  Converse in `us-east-1`, without seeing the AWS region, physical model, or
  inference-profile ID.
- Bedrock API-key and SigV4 modes have separate authentication tests. Inbound
  Light credentials are never used as AWS credentials, and AWS credentials or
  signing material are absent from snapshots, logs, errors, audit payloads,
  and client responses.
- The required core passes model-list, Chat Completions, Responses, and
  embeddings conformance without enabling either native client facade.
- If `anthropic_messages` is enabled, official Claude Code completes the
  buffered, streaming, tool-use, and required token-counting flows in the
  pinned conformance profile through `/anthropic/v1` using a Light-issued
  credential.
- If `gemini_native` is enabled, the pinned Google Gen AI SDK or Gemini CLI
  fixtures call the advertised `/gemini` surface with explicit Light
  authentication headers.
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
- A disabled optional profile registers no public route and adds no request-path
  task, lookup, allocation, provider restriction, or fallback behavior.

## Provider References

- [OpenAI Responses API](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Codex custom model providers](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers)
- [Codex authentication](https://learn.chatgpt.com/docs/auth)
- [Claude API overview and authentication](https://platform.claude.com/docs/en/api/overview)
- [Claude Code gateway guidance](https://code.claude.com/docs/en/llm-gateway)
- [Claude OpenAI SDK compatibility and limitations](https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk)
- [Amazon Bedrock Claude Sonnet 4.6 model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-sonnet-4-6.html)
- [Amazon Bedrock Converse API](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html)
- [Amazon Bedrock inference profiles](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-profiles-use.html)
- [Amazon Bedrock Anthropic Messages API](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-messages-api.html)
- [Amazon Bedrock Runtime endpoints](https://docs.aws.amazon.com/bedrock/latest/userguide/endpoints.html)
- [xAI inference API](https://docs.x.ai/developers/rest-api-reference/inference/chat)
- [xAI API-key authorization](https://docs.x.ai/developers/rest-api-reference/management/auth)
- [Gemini API reference](https://ai.google.dev/api)
- [Gemini gateway integration trade-offs](https://ai.google.dev/gemini-api/docs/partner-integration)
- [Gemini Interactions API](https://ai.google.dev/api/interactions-api)
- [Google Gen AI SDK custom base URL](https://googleapis.github.io/python-genai/#custom-base-url)
- [Vertex AI Gemini quickstart and ADC](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/start/quickstart)
