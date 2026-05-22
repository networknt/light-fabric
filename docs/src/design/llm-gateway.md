# LLM Gateway

## Status

Design proposal. The current `light-agent` runtime selects one active model
provider from `model-provider.yml`. That is acceptable if the selected provider
is an LLM gateway endpoint. The agent does not need to know how many upstream
providers the gateway can reach.

## Purpose

The LLM gateway is a centralized model access layer for agents and services.
Instead of each agent carrying credentials, endpoint details, routing rules,
and provider fallback logic, each agent calls one gateway endpoint. The gateway
then routes the request to OpenAI, Azure OpenAI, Bedrock, Anthropic, Gemini,
Ollama, Codex, or another provider based on agent configuration, model policy,
prompt characteristics, capability requirements, health, cost, and compliance
constraints.

This keeps `light-agent` simple and matches the current bootstrap model:

1. `startup.yml` is local.
2. Runtime configuration is fetched from config-server.
3. The agent loads one model provider after bootstrap.
4. That provider can be an OpenAI-compatible LLM gateway.
5. The gateway owns multi-provider fan-out.

## Goals

- Keep agents configured with one active model endpoint.
- Support many upstream LLM providers at the gateway at the same time.
- Allow provider routing by agent, service id, environment, prompt intent,
  requested capability, logical model name, cost, latency, region, and health.
- Keep provider credentials out of agent pods and agent config.
- Preserve the existing `model-provider` abstraction for direct provider access
  and reuse it inside the gateway where useful.
- Expose a provider-compatible HTTP API so existing agents can use the gateway
  without a new SDK.
- Support normal light-fabric bootstrap, config-server overrides, module
  registry visibility, config reload, controller registration, and audit.
- Make gateway decisions explainable enough for operations and compliance.

## Non-Goals

- Do not make `light-agent` load many providers directly for this use case.
  Multi-provider routing belongs in the gateway.
- Do not require every agent to understand provider-specific fields such as AWS
  region, Azure deployment name, or Anthropic max token settings.
- Do not expose upstream provider secrets through `tools/list`, diagnostics, or
  agent-visible configuration.
- Do not depend on an LLM classification call for every routing decision. The
  gateway should support deterministic routing first and optional classifier
  routing later.
- Do not merge the LLM gateway with the MCP router. The LLM gateway routes
  model calls; the MCP router routes tool calls.

## Relationship To Existing Components

### Light-Agent

`light-agent` should continue to select one model provider after runtime
bootstrap. For an LLM gateway deployment, the selected provider is the gateway:

```yaml
model-provider.provider: compatible
model-provider.model: agent-default
compatible.name: llm-gateway
compatible.baseUrl: https://llm-gateway.light-gateway:8443/v1
compatible.apiKey: ${secret.llmGatewayApiKey}
```

The `model-provider.model` value becomes a logical model name. It does not need
to be an upstream provider model id. Examples:

```yaml
model-provider.model: agent-default
model-provider.model: fast
model-provider.model: reasoning
model-provider.model: coding
model-provider.model: pii-safe
```

The gateway maps the logical model to a physical provider and model.

### Light-Gateway

The LLM gateway should be implemented as a `light-gateway` product capability,
activated by handler/config. This keeps LLM egress under the same gateway
family that already handles MCP routing, auth, rule execution, metrics,
service discovery, bootstrap, and reload.

The first implementation can expose an OpenAI-compatible endpoint:

```text
POST /v1/chat/completions
```

That is enough for `CompatibleProvider` and many external clients. Later phases
can add:

```text
POST /v1/responses
GET  /v1/models
```

### Model Provider Crate

The gateway can reuse `crates/model-provider` for upstream calls. The crate
already contains concrete providers and meta-providers:

- OpenAI
- Azure OpenAI
- Anthropic
- Bedrock
- Codex
- Compatible
- Gemini
- GLM
- Ollama
- OpenRouter
- Telnyx
- Copilot
- CLI providers where operationally appropriate
- `RouterProvider`
- `ReliableProvider`

For the gateway, direct concrete providers are upstream adapters. Routing and
fallback should be controlled by gateway config and policy, not by each agent.

## Request Flow

```text
agent
  -> LLM provider trait
  -> CompatibleProvider
  -> light-gateway /v1/chat/completions
  -> auth, correlation, policy, rate limit
  -> LLM route decision
  -> upstream provider adapter
  -> upstream LLM provider
  -> normalized response
  -> audit, metrics, token usage
  -> agent
```

The agent sees one model provider. The gateway sees the full routing context.

## Routing Inputs

The gateway should make routing decisions from a combination of trusted inputs:

- Authenticated caller identity from JWT, mTLS, or gateway-authenticated
  service registration.
- Agent metadata such as host id, agent definition id, service id, environment,
  tenant, and account.
- Logical model name from the request body.
- Request capabilities: tool calling, vision, JSON mode, long context,
  reasoning, streaming, prompt caching.
- Prompt features: intent keywords, size, language, sensitivity markers,
  coding vs support vs workflow execution.
- Configured policy: allowed providers, blocked providers, region constraints,
  cost tier, data residency, fallback chain.
- Runtime health: provider availability, error rate, latency, quota pressure.

If metadata is supplied as headers, the gateway should only trust those headers
from authenticated internal clients. Otherwise it should derive identity from
the token or connection.

Suggested internal headers:

```text
X-Light-Request-Id
X-Light-Service-Id
X-Light-Env-Tag
X-Light-Agent-Host-Id
X-Light-Agent-Definition-Id
X-Light-Tenant-Id
```

## Routing Stages

Routing should be deterministic before it is intelligent.

1. Explicit route

   If the request asks for a logical model with a direct configured route, use
   that route.

2. Agent policy

   Apply policy for the authenticated agent or service. This can narrow the
   allowed logical models and upstream providers.

3. Capability filter

   Remove upstreams that cannot satisfy required capabilities such as tools,
   vision, long context, or streaming.

4. Prompt classifier

   Optionally classify the prompt into a routing domain such as `fast`,
   `reasoning`, `coding`, `customer-support`, or `restricted-data`.

5. Cost and latency preference

   Choose the cheapest or fastest provider that satisfies policy and
   capability constraints.

6. Health and fallback

   If the selected upstream is unhealthy or returns a retryable error, follow a
   configured fallback chain.

## Gateway Configuration

The gateway should use a dedicated config file, for example
`llm-gateway.yml`, loaded through the same runtime config layering as other
light-fabric modules.

Example:

```yaml
enabled: ${llm-gateway.enabled:true}
pathPrefix: ${llm-gateway.pathPrefix:/v1}
defaultRoute: ${llm-gateway.defaultRoute:agent-default}

routes:
  agent-default:
    provider: openai-prod
    model: gpt-4o
    fallbacks:
      - provider: bedrock-us
        model: anthropic.claude-3-5-sonnet-20240620-v1:0

  fast:
    provider: openai-prod
    model: gpt-4o-mini

  reasoning:
    provider: bedrock-us
    model: anthropic.claude-3-7-sonnet-20250219-v1:0
    requiredCapabilities:
      - tools
      - long-context

providers:
  openai-prod:
    type: openai
    baseUrl: ${llm.openai.baseUrl:https://api.openai.com/v1}
    apiKey: ${llm.openai.apiKey:}
    maxTokens: ${llm.openai.maxTokens:}
    costTier: medium
    regions:
      - global

  bedrock-us:
    type: bedrock
    region: ${llm.bedrock.region:us-east-1}
    accessKeyId: ${llm.bedrock.accessKeyId:}
    secretAccessKey: ${llm.bedrock.secretAccessKey:}
    sessionToken: ${llm.bedrock.sessionToken:}
    costTier: high
    regions:
      - us-east-1

agentPolicies:
  com.networknt.agent.account-1.0.0:
    defaultRoute: agent-default
    allowedRoutes:
      - agent-default
      - fast
      - reasoning
    blockedProviders: []
    dataResidency:
      allowedRegions:
        - us-east-1
        - global

fallback:
  maxRetries: ${llm-gateway.fallback.maxRetries:1}
  baseBackoffMs: ${llm-gateway.fallback.baseBackoffMs:100}
```

The exact schema can evolve, but the important boundary is stable:

- Agent config points to one gateway endpoint.
- Gateway config owns provider inventory and route policy.
- Provider secrets are masked in module registry output.

## Provider Inventory

Each configured provider should have:

- A stable provider id.
- A provider type.
- Provider-specific connection settings.
- Supported capabilities.
- Allowed regions.
- Cost tier.
- Timeout and retry settings.
- Optional quota metadata.
- Optional tenant or account restrictions.

Provider ids should be operational names, not user-visible model names:

```yaml
providers:
  openai-prod:
    type: openai
  openai-eu:
    type: azure-openai
  bedrock-us:
    type: bedrock
  local-ollama:
    type: ollama
```

Logical model names are route names. They are safe for agents to request.

## Request And Response Contract

The first API should be OpenAI-compatible enough for `CompatibleProvider`:

```http
POST /v1/chat/completions
Authorization: Bearer <agent-or-service-token>
Content-Type: application/json
```

Request:

```json
{
  "model": "agent-default",
  "messages": [
    {"role": "system", "content": "You are a support agent."},
    {"role": "user", "content": "Help me investigate this account."}
  ],
  "temperature": 0.7,
  "tools": []
}
```

Response:

```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "..."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 1200,
    "completion_tokens": 240,
    "total_tokens": 1440
  },
  "light_gateway": {
    "route": "agent-default",
    "provider": "openai-prod",
    "model": "gpt-4o"
  }
}
```

The `light_gateway` field should be optional and controlled by diagnostics
policy. It is useful for internal debugging, but may be hidden from external
clients.

## Tool Calling

Tool calling remains an agent responsibility, but model-native tool-call
generation flows through the LLM gateway.

The LLM gateway should:

- Accept OpenAI-style tool definitions from the agent.
- Convert tool definitions to the upstream provider's native format when
  possible.
- Normalize provider tool-call responses back to the OpenAI-compatible shape.
- Return clear errors when a route cannot support tool calling.

The gateway should not execute MCP tools. The agent still calls
`light-gateway` MCP endpoints for `tools/list` and `tools/call`.

## Security

The gateway becomes the model egress control point, so it must enforce:

- Authentication for every model request.
- Authorization for logical models and provider routes.
- Tenant isolation.
- Provider allowlists and denylists.
- Secret masking in module registry and diagnostics.
- Optional request/response redaction or tokenization hooks.
- Data residency rules.
- Rate limits by tenant, agent, route, and provider.
- Audit records for route selection and usage.

Provider credentials should live only in gateway config or the secret system
feeding config-server. They should not be copied into agent config.

## Observability

Each gateway model call should produce structured telemetry:

- Request id.
- Caller identity.
- Agent id and service id when available.
- Logical model.
- Selected provider and physical model.
- Routing reason.
- Fallback attempts.
- Prompt and completion token counts.
- Latency by stage.
- Provider status code and error class.
- Cache hit or prompt-cache usage where available.
- Policy decisions.

Metrics should support dashboards by route, provider, tenant, and agent.

## Config Reload

The gateway should register `llm-gateway.yml` in the module registry and support
runtime reload.

Reload should be atomic:

1. Load and validate the new config.
2. Build provider clients and route tables.
3. Reject invalid route references before swapping state.
4. Swap active routing state.
5. Keep in-flight requests on the old state.

Validation should catch:

- Unknown provider ids.
- Unknown provider types.
- Routes without a provider.
- Fallbacks pointing to missing providers.
- Logical routes that require capabilities no provider can satisfy.
- Missing required provider settings for active routes.

## Agent Configuration Pattern

For a direct provider deployment:

```yaml
model-provider.provider: bedrock
model-provider.model: anthropic.claude-3-5-sonnet-20240620-v1:0
bedrock.region: us-east-1
```

For a gateway deployment:

```yaml
model-provider.provider: compatible
model-provider.model: agent-default
compatible.name: llm-gateway
compatible.baseUrl: https://llm-gateway.light-gateway:8443/v1
compatible.apiKey: ${llmGateway.agentApiKey}
```

The second form is the preferred enterprise model once centralized routing is
available.

## Phased Implementation

### Phase 1: OpenAI-Compatible Gateway Endpoint

- Add `llm-gateway.yml`.
- Add a `light-gateway` handler for `/v1/chat/completions`.
- Support non-streaming OpenAI-compatible requests and responses.
- Route by logical model name to one configured upstream provider.
- Mask provider secrets in module registry.
- Add basic audit and metrics.

### Phase 2: Policy, Fallback, And Reload

- Add per-agent route policy.
- Add health-aware fallback chains.
- Support runtime reload with atomic state swap.
- Add diagnostics endpoint or module registry details for active routes.

### Phase 3: Capability-Aware Routing

- Add capability metadata for each provider route.
- Route by tools, vision, long context, streaming, and JSON mode.
- Normalize tool-call request and response shapes across providers.

### Phase 4: Prompt-Aware Routing

- Add deterministic prompt classifiers.
- Add optional lightweight model or embedding classifier for complex routing.
- Record routing reasons for audit.

### Phase 5: Advanced Provider Features

- Add streaming.
- Add `/v1/responses`.
- Add prompt caching hints.
- Add quota-aware routing.
- Add data redaction or tokenization hooks when the tokenization service
  contract is finalized.

## Open Questions

- Should the first implementation live in `light-pingora` as a handler module
  or in a new `llm-gateway` crate used by `light-gateway`?
- Should logical model policy be stored only in config-server values, or also
  managed by portal database tables for runtime UI edits?
- Should gateway diagnostics expose selected provider/model to agents, or only
  to operators?
- Should prompt-aware routing use Light-Rule first, a dedicated classifier, or
  both?
- How should provider quota information be collected for cloud providers that
  do not expose uniform quota APIs?

## Decision

Use the LLM gateway as the single model provider endpoint for enterprise
agents. `light-agent` stays single-provider from its own point of view. The
gateway owns multiple upstream providers, route selection, fallback,
credentials, policy, audit, and observability.
