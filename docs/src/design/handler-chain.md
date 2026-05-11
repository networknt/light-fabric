# Handler Chain

Status: Phases 1, 2, 3, 4, 5, 6, 7, and 8 implemented; further transport phases proposed

## Purpose

Light Fabric needs a `light-pingora` handler chain for the Rust
`light-gateway` product.

The first implementation should focus on `light-pingora`, not a generic
cross-framework abstraction. A Pingora-first design is simpler and matches the
gateway family of use cases: gateway, sidecar, proxy server, proxy client, load
balancer, and BFF.

The deployment model should use one `light-gateway` binary. Different runtime
behaviors should come from product-specific configuration managed in
`light-portal` and delivered by config-server. A BFF deployment, a sidecar
deployment, and a load-balancer deployment can therefore run the same binary
with different `handler.yml`, traffic/resource config, and handler-specific
config files.

The design should preserve the useful part of `light-4j` `handler.yml`: ordered
configuration of cross-cutting request and response concerns. It should not copy
the Java reflection model, mutable `next` handler pattern, or class-name-based
configuration.

## Goals

- Add middleware handler-chain support to `frameworks/light-pingora`.
- Use one `apps/light-gateway` binary for the Pingora gateway family.
- Keep `handler.yml` as the chain and ordering configuration.
- Let `light-portal` manage product-specific configuration and config-server
  deliver it at startup.
- Support virtual hosts selected from the HTTP `Host` header.
- Serve static SPA content directly from Pingora.
- Proxy API, BFF, sidecar, and balancer routes to upstream services.
- Use stable handler IDs instead of Rust type names.
- Use explicit handler registration. Do not require `inventory`.
- Integrate loaded handler and traffic/resource config with `ModuleRegistry`.
- Keep the design compatible with runtime config reload.

## Non-Goals

- Do not build a transport-neutral `light-handler` crate in the first phase.
- Do not add an Axum/Tower adapter in the first phase.
- Do not create separate binaries for gateway, sidecar, proxy server, proxy
  client, load balancer, and BFF in the first phase.
- Do not dynamically load handler crates from `handler.yml`.
- Do not use Java-style reflection or string-to-type construction.
- Do not make Rust type names part of the public config contract.
- Do not support multi-certificate TLS SNI selection in the first phase.
- Do not implement streaming static-file delivery in the first phase unless it
  is needed for a concrete SPA asset size problem.

## Current Shape

`light-pingora` already adapts a Pingora proxy into the shared runtime:

```rust
pub trait PingoraApp: Send + Sync + 'static {
    type Proxy: ProxyHttp + Send + Sync + 'static;

    fn proxy(&self, config: &RuntimeConfig) -> Result<Self::Proxy, RuntimeError>;
}
```

`PingoraTransport` calls `app.proxy(config)` and passes the result to
`pingora::proxy::http_proxy_service(...)`.

Pingora's `ProxyHttp` lifecycle already has the hooks needed for the gateway
family:

- `request_filter`: validate, authenticate, rate limit, or directly write a
  local response such as a static file
- `upstream_peer`: select the upstream for proxy routes
- `upstream_request_filter`: mutate the request sent to upstream
- `upstream_response_filter`: mutate the upstream response before caching
- `response_filter`: mutate the response sent to the browser

The current `light-gateway` already writes `/health` directly from
`request_filter`. Static SPA serving can use the same pattern.

## Product Model

The Rust `light-gateway` binary should link all built-in Pingora gateway
capabilities:

- virtual host routing
- static SPA serving
- reverse proxy routing
- outbound proxy behavior
- upstream load balancing
- sidecar token/header behavior
- shared middleware handlers

The active behavior is selected by configuration, not by compiling a different
binary. The six product personas are configuration profiles:

- `gateway`
- `sidecar`
- `proxy-server`
- `proxy-client`
- `balancer`
- `bff`

These profiles can be represented in `light-portal` as product-specific config
sets. At runtime, `light-gateway` only sees the resolved files returned by
config-server. The binary should not need to know whether the files came from a
portal product template, an environment override, or a local fallback.

This keeps deployment simple:

- one binary
- one container image
- one `light-pingora` framework
- different behavior by remote config

The tradeoff is that config validation must be strong. A product config should
not silently start in a different mode if a static root, virtual host, upstream,
or chain is wrong.

## High-Level Flow

The Pingora gateway request flow should be:

```text
request
  -> match handler.yml paths by path and method
  -> fall back to handler.yml defaultHandlers when no path matches
  -> run request handlers
  -> proxy fixed upstream, route by service_id/service_url, serve static file,
     or return error
  -> run response handlers
  -> response
```

For static handlers such as virtual-host or path-resource, `request_filter`
writes the response and returns `Ok(true)` so Pingora does not proxy the
request.

For proxy or router handlers, `request_filter` stores the selected upstream
decision in the per-request context and returns `Ok(false)`. `upstream_peer`
and `upstream_request_filter` then use that context to connect to the right
upstream and set headers.

## Crate Layout

Keep the first implementation inside `frameworks/light-pingora`.

Suggested modules:

```text
frameworks/light-pingora/src/
  lib.rs
  handler.rs
  correlation.rs
  cors.rs
  metrics.rs
  proxy.rs
  resource.rs
  router.rs
  service.rs
  token.rs
```

Responsibilities:

- parse and validate `handler.yml`
- parse `handler.yaml` as a compatibility fallback
- parse and validate `proxy.yml`, `router.yml`, `path-resource.yml`, and
  `virtual-host.yml`
- build explicit handler registry
- resolve handler chains
- match handler paths and fallback handlers
- capture Java-style `{name}` path-template variables
- load active handler-specific config files
- serve static SPA content
- select fixed proxy upstreams from `proxy.yml`
- select dynamic sidecar/router upstreams from `router.yml`
- resolve sidecar `service_id` values from `pathPrefixService.yml`
- retrieve and cache OAuth client-credentials tokens from `client.yml`
- expose module-registry entries for active handler and traffic/resource config

This keeps the first implementation close to the Pingora lifecycle and avoids
premature abstractions for Axum.

If Axum later needs the same handler semantics, extract the framework-neutral
parts after the Pingora implementation has stabilized.

## Configuration Split

Use `handler.yml` for the Java-compatible handler middleware contract:
handler declarations, reusable chains, path-to-chain mappings, and fallback
handlers.

Use Java-compatible product-specific config files for traffic and static
resource behavior:

- `proxy.yml`: fixed inbound reverse proxy targets for gateway, proxy server,
  balancer, and simple BFF API forwarding.
- `router.yml`: dynamic outbound routing by `service_id` or `service_url`,
  mainly for sidecar-style deployments.
- `path-resource.yml` or `path-resource.yaml`: a single static resource mount.
- `virtual-host.yml` or `virtual-host.yaml`: host-based static resource mounts
  for BFF/SPA deployments.

The product profile selected in `light-portal` decides which of these files are
included and which handlers are active in `handler.yml`. The Rust binary should
not require a separate `gateway.yml` to duplicate these existing contracts.

Handler-specific files such as `correlation.yml`, `cors.yml`, `metrics.yml`,
`header.yml`, `security.yml`, `apikey.yml`, `basic-auth.yml`,
`unified-security.yml`, and `limit.yml` stay separate. They are loaded only
when the corresponding handler is active in the resolved path/default
execution model. Phase 3 implements this active loading for `correlation.yml`,
`cors.yml`, and `metrics.yml`. Phase 4 extends the same active-loading and
reload model to `header.yml`, `security.yml`, `apikey.yml`,
`basic-auth.yml`, `unified-security.yml`, and `limit.yml`.

### Remote Config Source

`light-gateway` starts with enough local bootstrap configuration to contact
config-server. The existing Light Fabric runtime then resolves local and remote
configuration before `light-pingora` builds the runtime handler/resource/proxy
model.

Startup flow:

1. load local bootstrap files from the configured config directory
2. contact config-server using the configured service identity, environment,
   and authorization
3. download remote product configuration managed by `light-portal`
4. merge remote config with local fallback config
5. load `handler.yml`, applicable traffic/resource config files, and active
   handler-specific config files
6. validate the complete route and handler model
7. bind Pingora listeners
8. register the runtime instance with the controller

The remote product config should include:

- `handler.yml`
- `proxy.yml` for fixed inbound proxy profiles
- `router.yml` for sidecar/router profiles
- `path-resource.yml` or `virtual-host.yml` for static/BFF profiles
- active handler config files
- TLS, trust, or client files required by the runtime
- optional product-specific static file references or mount paths

`handler.yml` decides which linked handlers are active. A handler that is
registered in the binary but not referenced by any configured `paths` entry or
`defaultHandlers` chain should not be instantiated, should not load its config
file, and should never run.

### Handler Config

Example `handler.yml`:

```yaml
enabled: ${handler.enabled:true}
reportHandlerDuration: ${handler.reportHandlerDuration:false}
handlerMetricsLogLevel: ${handler.handlerMetricsLogLevel:DEBUG}
basePath: ${handler.basePath:/}
handlers: ${handler.handlers:[]}
chains: ${handler.chains:{}}
paths: ${handler.paths:[]}
defaultHandlers: ${handler.defaultHandlers:[]}
```

The config-server values managed by `light-portal` provide the concrete arrays
and maps:

```yaml
handler.handlers:
  - correlation
  - headers
  - metrics
  - cors
  - jwt
  - rate-limit

handler.chains:
  spa:
    exec:
      - correlation
      - headers
      - metrics
      - cors
  api:
    exec:
      - correlation
      - headers
      - metrics
      - cors
      - jwt
      - rate-limit
  public:
    exec:
      - correlation
      - headers
      - metrics

handler.paths:
  - path: /api/
    method: GET
    exec:
      - api

handler.defaultHandlers:
  - public
```

This keeps the same top-level `handler.yml` contract as the Java framework:
`enabled`, `reportHandlerDuration`, `handlerMetricsLogLevel`, `basePath`,
`handlers`, `chains`, `paths`, and `defaultHandlers`.

The Rust implementation also accepts the Java extension fields
`additionalHandlers`, `additionalChains`, and `additionalPaths`. They are
merged into the effective handler model before validation.

Unlike Java, the Rust `handlers` list uses stable short handler IDs. It does
not use fully qualified class names, and it does not need `@alias` because the
IDs are already short and stable.

`handler.yml` is the preferred Rust file name. `handler.yaml` is accepted as a
compatibility fallback because some Java modules and templates use that suffix.

### Fixed Proxy Config

`proxy.yml` should keep the Java inbound reverse-proxy contract. It is used
when the deployment has a known set of target upstream URIs.

```yaml
enabled: ${proxy.enabled:true}
http2Enabled: ${proxy.http2Enabled:false}
hosts: ${proxy.hosts:http://localhost:8080}
connectionsPerThread: ${proxy.connectionsPerThread:20}
maxRequestTime: ${proxy.maxRequestTime:1000}
rewriteHostHeader: ${proxy.rewriteHostHeader:true}
reuseXForwarded: ${proxy.reuseXForwarded:false}
maxConnectionRetries: ${proxy.maxConnectionRetries:3}
maxQueueSize: ${proxy.maxQueueSize:0}
forwardJwtClaims: ${proxy.forwardJwtClaims:false}
metricsInjection: ${proxy.metricsInjection:false}
metricsName: ${proxy.metricsName:proxy-response}
```

The Rust implementation should parse `proxy.hosts` as one or more comma
separated `http://` or `https://` targets and select a target with round-robin
load balancing. It should preserve `rewriteHostHeader`, `reuseXForwarded`,
request timeout, retry, and queue settings where Pingora exposes equivalent
behavior.

### Router Config

`router.yml` should keep the Java outbound router contract. This is primarily
for the sidecar pattern, where earlier handlers resolve `service_id`,
`service_url`, tokens, and discovery context before the router connects to the
downstream service.

```yaml
http2Enabled: ${router.http2Enabled:true}
httpsEnabled: ${router.httpsEnabled:true}
maxRequestTime: ${router.maxRequestTime:1000}
pathPrefixMaxRequestTime: ${router.pathPrefixMaxRequestTime:{}}
connectionsPerThread: ${router.connectionsPerThread:10}
softMaxConnectionsPerThread: ${router.softMaxConnectionsPerThread:5}
maxQueueSize: ${router.maxQueueSize:0}
rewriteHostHeader: ${router.rewriteHostHeader:true}
reuseXForwarded: ${router.reuseXForwarded:false}
maxConnectionRetries: ${router.maxConnectionRetries:3}
preResolveFQDN2IP: ${router.preResolveFQDN2IP:false}
hostWhitelist: ${router.hostWhitelist:[]}
serviceIdQueryParameter: ${router.serviceIdQueryParameter:false}
urlRewriteRules: ${router.urlRewriteRules:[]}
methodRewriteRules: ${router.methodRewriteRules:[]}
queryParamRewriteRules: ${router.queryParamRewriteRules:{}}
headerRewriteRules: ${router.headerRewriteRules:{}}
metricsInjection: ${router.metricsInjection:false}
metricsName: ${router.metricsName:router-response}
```

The Java router chooses the target from `service_url` first, guarded by
`hostWhitelist`, or from `service_id` plus optional `env_tag` through service
discovery.

Phase 5 implements the Pingora router execution path and keeps the Java
configuration shape. The active `router` handler loads and registers
`router.yml`, selects direct `service_url` targets after `hostWhitelist`
validation, supports `serviceIdQueryParameter`, and removes router selection
headers before forwarding upstream. It also applies Java-style URL, method,
query-parameter, and header rewrite rules.

Rust adds `serviceTargets` as an interim improvement for `service_id` routing:

```yaml
serviceTargets:
  com.networknt.petstore-1.0.0:
    - http://localhost:8080
  com.networknt.petstore-1.0.0|dev:
    - https://petstore-dev.example.com
```

This lets sidecar-style router flows run in local/static deployments and acts
as the fallback when controller discovery is unavailable.

Phase 6 adds the sidecar path-prefix and token flow. Phase 7 adds
controller-backed `service_id` discovery while keeping the same request
contract and the same static fallback.

### Sidecar Path Prefix And Token Config

`pathPrefixService.yml` maps request path prefixes to downstream service IDs.
The handler writes `service_id` only when the request does not already provide
one.

```yaml
enabled: ${pathPrefixService.enabled:true}
mapping: ${pathPrefixService.mapping:{}}
```

Rust intentionally selects the longest path-boundary prefix. This avoids map
iteration ambiguity when prefixes overlap and prevents `/v1/address` from
matching `/v1/address2`.

`token.yml` gates when the token handler should run:

```yaml
enabled: ${token.enabled:false}
appliedPathPrefixes: ${token.appliedPathPrefixes:}
```

The token handler reads the Java-compatible client credentials section from
`client.yml`:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
oauth:
  multipleAuthServers: ${client.multipleAuthServers:false}
  token:
    cache:
      capacity: ${client.tokenCacheCapacity:200}
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    server_url: ${client.tokenServerUrl:}
    serviceId: ${client.tokenServiceId:com.networknt.oauth2-token-1.0.0}
    proxyHost: ${client.tokenProxyHost:}
    proxyPort: ${client.tokenProxyPort:}
    enableHttp2: ${client.tokenEnableHttp2:true}
    client_credentials:
      uri: ${client.tokenCcUri:/oauth2/token}
      client_id: ${client.tokenCcClientId:}
      client_secret: ${client.tokenCcClientSecret:}
      scope: ${client.tokenCcScope:}
      serviceIdAuthServers: ${client.tokenCcServiceIdAuthServers:}
pathPrefixServices: ${client.pathPrefixServices:}
request:
  connectTimeout: ${client.connectTimeout:2000}
  timeout: ${client.timeout:3000}
  enableHttp2: ${client.enableHttp2:true}
```

In single-auth-server mode, the handler uses the configured token server and
client credentials for all matched paths. In `multipleAuthServers` mode, it
uses `service_id` or `pathPrefixServices` to select
`client_credentials.serviceIdAuthServers[service_id]`.

The token request follows the Java request shape:

- `POST` to `server_url + uri`
- `Content-Type: application/x-www-form-urlencoded`
- `Accept: application/json`
- HTTP Basic authentication with `client_id:client_secret`
- form fields `grant_type=client_credentials` and optional space-joined
  `scope`

The injected header follows the Java gateway rule:

- if the inbound request has no `Authorization`, inject
  `Authorization: Bearer <token>`
- if the inbound request already has `Authorization`, inject
  `X-Scope-Token: Bearer <token>`

The Rust cache is local to the gateway process and is registered as
`light-pingora/token-cache` when a runtime cache registry is available. Cache
summaries expose key and expiry metadata but never expose bearer token values.
Tokens are refreshed synchronously inside the configured renew-before-expiry
window. Async background renewal can be added later if blocking refresh latency
becomes visible.

When `server_url` is not configured, phase 7 discovers the token service from
`serviceId` through the runtime portal-registry client. This requires
`server.enableRegistry` and a live controller registration. A disconnected
registry client returns a clear configuration/runtime error instead of silently
falling back to an unknown token endpoint.

### Static Resource Config

For a single static site, keep `path-resource.yml`:

```yaml
path: ${path-resource.path:/public}
base: ${path-resource.base:/opt/light-4j/public}
prefix: ${path-resource.prefix:true}
transferMinSize: ${path-resource.transferMinSize:1024}
directoryListingEnabled: ${path-resource.directoryListingEnabled:false}
```

For host-based BFF/static sites, keep `virtual-host.yml`:

```yaml
hosts: ${virtual-host.hosts:[]}
```

Example config-server values:

```yaml
virtual-host.hosts:
  - domain: local.localhost
    path: /
    base: /lightapi/dist
    transferMinSize: 10245760
    directoryListingEnabled: false
  - domain: signin.localhost
    path: /
    base: /signin/dist
    transferMinSize: 10245760
    directoryListingEnabled: false
```

Rust should preserve the Java `domain`, `path`, `base`, `transferMinSize`, and
`directoryListingEnabled` fields. It should also add the Rust improvement for
SPA fallback: when a static virtual host cannot find a requested browser route
and the path does not look like an asset, it should serve `index.html` from the
matched static root.

### BFF Wiring Example

The Java BFF config in `portal-config-loc/all-in-lt/light-gateway` uses
`handler.paths` to send API routes through the `default` chain, which includes
path-prefix service resolution, token handling, and the router. It then uses:

```yaml
handler.defaultHandlers:
  - cors
  - virtual
```

That means unmatched browser routes fall through to CORS plus virtual-host
static serving. Rust should keep this pattern: `handler.yml` decides whether a
request goes to proxy/router/static handling, based on paths and fallback
handlers.

Other product personas use different config file combinations. A BFF commonly
uses `handler.yml`, `router.yml`, path-prefix/token configs, and
`virtual-host.yml`. A simple proxy or balancer can use `handler.yml` and
`proxy.yml`. A sidecar uses `handler.yml`, `router.yml`, token/cache config,
registry/discovery config, and usually no static resource config.

### Phase 3 Handler Config

Phase 3 implements the first three Java-compatible cross-cutting handlers.

`correlation.yml`:

```yaml
enabled: ${correlation.enabled:true}
autogenCorrelationID: ${correlation.autogenCorrelationID:true}
correlationMdcField: ${correlation.correlationMdcField:cId}
traceabilityMdcField: ${correlation.traceabilityMdcField:tId}
```

The Rust handler reads `X-Correlation-Id` and `X-Traceability-Id`, generates a
Java-compatible URL-safe UUID value when correlation is missing, passes the
correlation ID to the upstream request, and echoes `X-Traceability-Id` on the
response. It stores the values in the Pingora request context instead of MDC.

`cors.yml`:

```yaml
enabled: ${cors.enabled:true}
allowedOrigins: ${cors.allowedOrigins:}
allowedMethods: ${cors.allowedMethods:}
pathPrefixAllowed: ${cors.pathPrefixAllowed:}
```

The Rust handler accepts the same list/string forms as Java, supports
`pathPrefixAllowed`, short-circuits preflight `OPTIONS`, rejects disallowed
origins with `403`, and adds the CORS response headers before static or proxied
responses are sent. Rust intentionally uses longest-prefix selection for
`pathPrefixAllowed` so overlapping prefixes are deterministic.

`metrics.yml`:

```yaml
enabled: ${metrics.enabled:true}
enableJVMMonitor: ${metrics.enableJVMMonitor:false}
serverProtocol: ${metrics.serverProtocol:http}
serverHost: ${metrics.serverHost:localhost}
serverPath: ${metrics.serverPath:/apm/metricFeed}
serverPort: ${metrics.serverPort:8086}
serverName: ${metrics.serverName:metrics}
serverUser: ${metrics.serverUser:admin}
serverPass: ${metrics.serverPass:admin}
reportInMinutes: ${metrics.reportInMinutes:1}
productName: ${metrics.productName:http-sidecar}
sendScopeClientId: ${metrics.sendScopeClientId:false}
sendCallerId: ${metrics.sendCallerId:false}
sendIssuer: ${metrics.sendIssuer:false}
issuerRegex: ${metrics.issuerRegex:}
```

Phase 3 parses and registers this config with `serverPass` masked, records
request counts and status classes in memory, and logs request metrics with the
matched endpoint and correlation ID. `enableJVMMonitor` is parsed for config
compatibility but is not applicable to Rust. External Influx/APM reporters are
deferred until the metrics sink decision is made.

### Phase 4 Handler Config

Phase 4 implements the security-oriented Java-compatible handlers that fit the
Pingora request metadata model.

`header.yml`:

```yaml
enabled: ${header.enabled:false}
request:
  remove: ${header.request.remove:}
  update: ${header.request.update:}
response:
  remove: ${header.response.remove:}
  update: ${header.response.update:}
pathPrefixHeader: ${header.pathPrefixHeader:}
```

The Rust handler applies request header remove/update rules before proxying and
response header remove/update rules before static or proxied responses are
sent. Rust intentionally uses longest-prefix selection for `pathPrefixHeader`
so overlapping prefixes are deterministic.

`apikey.yml`:

```yaml
enabled: ${apikey.enabled:true}
hashEnabled: ${apikey.hashEnabled:false}
pathPrefixAuths: ${apikey.pathPrefixAuths:[]}
```

The Rust handler follows the Java rule that no matching path prefix means the
handler passes the request. A matching rule validates the configured header
against either a plain API key or the Java `iterations:saltHex:hashHex`
PBKDF2-HMAC-SHA1 hash format.

`basic-auth.yml`:

```yaml
enabled: ${basic.enabled:false}
enableAD: ${basic.enableAD:true}
allowAnonymous: ${basic.allowAnonymous:false}
allowBearerToken: ${basic.allowBearerToken:false}
users: ${basic.users:[]}
```

The Rust handler supports configured local users, anonymous path users, and the
Java-compatible bearer pass-through mode. LDAP/AD authentication is parsed for
configuration compatibility but is not implemented in phase 4.

`security.yml`:

```yaml
enableVerifyJwt: ${security.enableVerifyJwt:true}
ignoreJwtExpiry: ${security.ignoreJwtExpiry:false}
enableH2c: ${security.enableH2c:false}
enableMockJwt: ${security.enableMockJwt:false}
jwt:
  certificate: ${security.jwt.certificate:{}}
  clockSkewInSeconds: ${security.jwt.clockSkewInSeconds:60}
  keyResolver: ${security.jwt.keyResolver:}
skipPathPrefixes: ${security.skipPathPrefixes:[]}
passThroughClaims: ${security.passThroughClaims:{}}
```

The Rust handler verifies Bearer JWTs with configured PEM certificates, honors
`kid` when present, supports RSA and EC algorithms handled by the Rust JWT
library, applies clock skew and optional expiry bypass, caches decoded claims,
and forwards configured pass-through claims as request headers. Dynamic JWK key
service bootstrap and SWT/SJWT verification are deferred until the runtime has
the discovery and key-service client surface needed by those flows.

`unified-security.yml`:

```yaml
enabled: ${unified-security.enabled:true}
anonymousPrefixes: ${unified-security.anonymousPrefixes:[]}
pathPrefixAuths: ${unified-security.pathPrefixAuths:[]}
```

The Rust handler supports Java-style path-prefix selection across Basic, JWT,
and API-key authentication. Anonymous prefixes bypass authentication. SWT/SJWT
rules return a clear not-implemented response until the discovery-backed key
flow is added.

`limit.yml`:

```yaml
enabled: ${limit.enabled:false}
concurrentRequest: ${limit.concurrentRequest:0}
queueSize: ${limit.queueSize:0}
errorCode: ${limit.errorCode:429}
rateLimit: ${limit.rateLimit:}
headersAlwaysSet: ${limit.headersAlwaysSet:false}
key: ${limit.key:server}
server: ${limit.server:{}}
address: ${limit.address:{}}
client: ${limit.client:{}}
user: ${limit.user:{}}
```

The Rust handler implements in-memory request rate limiting by server, client
address, JWT client ID, or JWT user ID. It emits `X-RateLimit-Limit`,
`X-RateLimit-Remaining`, `X-RateLimit-Reset`, and `Retry-After` when a request
is rejected, and it can always emit the rate-limit headers when
`headersAlwaysSet` is enabled. Cluster-wide distributed counters are deferred
until there is a concrete gateway clustering requirement.

## Handler Registry

Use explicit registration.

```rust
let handlers = PingoraHandlerRegistry::new()
    .register(correlation::descriptor())
    .register(headers::descriptor())
    .register(metrics::descriptor())
    .register(cors::descriptor())
    .register(jwt::descriptor())
    .register(rate_limit::descriptor());
```

No `inventory` is needed for the first version. Explicit registration is
deterministic, testable, and makes the compiled-in handler set clear from the
service binary.

The `light-gateway` binary can register every built-in handler it supports.
Registration only makes a handler available. Activation is controlled by
`handler.yml`.

Build the active handler set lazily:

1. parse `handler.yml`
2. resolve `paths` and `defaultHandlers`
3. expand any referenced chains
4. compute the set of referenced handler IDs
5. instantiate only referenced handlers
6. load config only for referenced handlers

This allows one binary to support gateway, sidecar, proxy, balancer, and BFF
profiles without requiring unused handler config files.

The registry maps stable config IDs to factories:

```rust
pub struct PingoraHandlerDescriptor {
    pub id: &'static str,
    pub kind: PingoraHandlerKind,
    pub factory: PingoraHandlerFactory,
}
```

Suggested first handler IDs:

- `correlation`
- `headers`
- `metrics`
- `cors`
- `jwt`
- `api-key`
- `basic-auth`
- `rate-limit`
- `request-size-limit`

Trace headers should be handled by `correlation`; there should not be a
separate `traceability` handler.

## Handler API

Use Pingora phases directly. Avoid a generic exchange abstraction until another
framework needs it.

The current implementation keeps `PingoraHandler` as a descriptor/factory
surface and executes the built-in phase 3 handlers from `light-gateway`'s
Pingora lifecycle. This keeps the first implementation straightforward:

- `request_filter` resolves the configured chain and runs request-stage
  handlers in order.
- A request-stage handler can continue, short-circuit with a local response, or
  select a terminal action such as proxy/static/health.
- `upstream_request_filter` applies upstream request mutations such as
  generated correlation IDs.
- `response_filter` applies response-stage headers and records proxied
  response metrics.
- Static responses call the same response decoration and metrics code before
  writing the local response.

Once security/rate-limit handlers are added, this can be lifted into a richer
trait with request/upstream/response hooks if the duplication becomes real. It
is intentionally not generalized before the Pingora behavior stabilizes.

Response handlers should run before both static and proxied responses are sent.
For proxied responses, this maps to Pingora `response_filter`. For static
responses, the static-file renderer calls the same response handler chain
before writing the local response.

## Request Context

The per-request context should carry route decisions across Pingora phases.

```rust
pub struct GatewayRequestContext {
    pub upstream: Option<ProxyTarget>,
    pub endpoint: String,
    pub method: String,
    pub path_params: BTreeMap<String, String>,
    pub correlation: CorrelationState,
    pub cors: Option<CorsResponseHeaders>,
    pub metrics_enabled: bool,
}
```

The context is created by `ProxyHttp::new_ctx()` and populated in
`request_filter`.

`upstream_peer` should only select an upstream after a proxy or router handler
has selected one. If no upstream is selected for a proxied request, the
implementation should return a clear configuration error rather than silently
falling back.

## Virtual Hosts

Virtual-host static serving should use the HTTP `Host` header.

Host normalization rules:

- lowercase the host
- strip the port when present
- reject empty or invalid hosts unless a default virtual host is configured
- exact host match first
- wildcard match such as `*.example.com` after exact hosts, with the longest
  matching suffix winning

HTTP host routing is enough for the first implementation.

TLS certificate selection by SNI is separate. The current `light-pingora`
transport uses one Rustls TLS setting for the listener, so the first production
options are:

- terminate TLS at ingress or a load balancer
- use a wildcard certificate
- use one certificate with all required SANs

Phase 8 evaluated dynamic multi-cert SNI selection. The current
`light-pingora` build uses Pingora's Rustls listener, and Pingora 0.8 Rustls
TLS settings do not support certificate callbacks. For now the production
options remain terminating TLS before `light-gateway`, using a wildcard
certificate, or using one certificate with all required SANs. Native multi-cert
SNI can be added only after moving to a Pingora TLS backend/version that
supports server certificate callbacks or certificate resolution through Rustls.

## Static SPA Rendering

Static SPA rendering should be part of the Pingora resource engine, not a
generic middleware handler. It is enabled by `path-resource.yml` or
`virtual-host.yml`, typically for BFF profiles.

Rules for the first implementation:

- support `GET` and `HEAD`
- return `405` for unsupported methods on static routes
- canonicalize requested paths under the configured static root
- reject path traversal
- do not serve files outside the static root
- deny dotfiles by default
- do not list directories
- serve `index.html` for the root path
- support SPA fallback to `index.html` for non-asset routes
- infer `Content-Type` from file extension
- set `Cache-Control: no-cache` for `index.html`
- set long immutable cache headers for hashed assets
- allow static route prefixes to be bypassed by API routes such as `/api/`,
  `/oauth/`, `/mcp/`, or `/ws/`

Recommended cache behavior:

```text
index.html                 Cache-Control: no-cache
*.js, *.css with hash       Cache-Control: public, max-age=31536000, immutable
images/fonts with hash      Cache-Control: public, max-age=31536000, immutable
other assets                Cache-Control: public, max-age=3600
```

Phase 8 keeps small static files on the simple read-then-write path and streams
files whose size is greater than or equal to the configured
`transferMinSize`. Static responses include `ETag` and `Last-Modified`, honor
`If-None-Match` and `If-Modified-Since`, and return `304` without a response
body when the browser cache is current.

## Proxy And Router Behavior

`proxy.yml` selects from configured upstream URIs. This is the simpler inbound
reverse-proxy case and should be implemented before dynamic sidecar routing.

Fixed proxy target behavior:

- parse comma-separated `proxy.hosts`
- support `http://` and `https://`
- duplicate a single host internally if retry/load-balancer behavior needs at
  least two entries
- select upstream with round-robin
- apply timeout, retry, queue, and host-forwarding settings where Pingora
  supports them

`router.yml` selects from request metadata. Phase 5 implements direct
`service_url` targets, static `serviceTargets` for `service_id`, host whitelist
enforcement, and rewrite behavior. Phase 7 adds controller-backed
`service_id` lookup through the runtime portal-registry client and keeps static
`serviceTargets` as a local fallback.

Router target behavior:

- prefer `service_url` when present and allowed by `router.hostWhitelist`
- otherwise use `service_id` plus optional `env_tag`
- optionally allow `service_id` from the query string when
  `serviceIdQueryParameter` is true
- resolve `service_id` from controller discovery when the portal-registry
  client is connected
- fall back to `router.serviceTargets` for local/static deployments or
  controller lookup failures
- support URL, method, query-parameter, and header rewrite rules
- remove `service_url` and `service_id` headers before forwarding

`upstream_peer` creates the `HttpPeer` from the selected upstream:

- address
- TLS enabled
- SNI
- optional host header

`upstream_request_filter` should set or override upstream headers such as:

- `Host`
- `X-Forwarded-For`
- `X-Forwarded-Proto`
- `X-Forwarded-Host`
- `X-Light-Gateway` or equivalent runtime marker

Handler-specific upstream mutations should also run from this phase.

## Chain Resolution

Startup should validate handler and selected traffic/resource configuration
before binding listeners.

Validation rules:

- every handler ID in `handler.yml` must exist in the explicit registry
- every chain item must resolve to a registered handler or another chain
- recursive chain references are invalid
- every `handler.paths` entry must reference existing chains or handlers
- every `handler.defaultHandlers` entry must reference existing chains or
  handlers
- `proxy.yml` hosts must be valid `http://` or `https://` URIs when the proxy
  handler is active
- `router.yml` rewrite rules must be parseable when the router handler is
  active
- every static virtual host must have a static root
- static roots must be absolute or resolved relative to a configured base
- duplicate exact virtual hosts are invalid
- duplicate handler IDs in the registry are invalid

The resolved model should be immutable and cheap to read:

```rust
pub struct GatewayRuntimeModel {
    pub virtual_hosts: BTreeMap<String, Arc<VirtualHost>>,
    pub default_host: Option<Arc<VirtualHost>>,
    pub chains: BTreeMap<String, Arc<ResolvedHandlerChain>>,
    pub proxy_targets: Vec<Arc<ProxyTarget>>,
}
```

Config reload should continue to swap loaded models atomically. In-flight
requests should keep using the handler/resource/proxy/router model they already
selected.

## Runtime Integration

`light-runtime` remains responsible for bootstrap, config loading, lifecycle,
controller registration, and module registry. `light-pingora` should load its
Pingora-specific handler, traffic, and resource config through the existing
runtime config loader.

Module IDs:

- `light-pingora/handler`
- `light-pingora/proxy`
- `light-pingora/router`
- `light-pingora/path-prefix-service`
- `light-pingora/token`
- `light-client/client`
- `light-pingora/path-resource`
- `light-pingora/virtual-host`
- `light-pingora/correlation`
- `light-pingora/cors`
- `light-pingora/metrics`
- `light-pingora/header`
- `light-pingora/security`
- `light-pingora/apikey`
- `light-pingora/basic-auth`
- `light-pingora/unified-security`
- `light-pingora/limit`

The module registry should expose:

- handler config snapshot, masked
- proxy, router, path-resource, and virtual-host config snapshots, masked
- active handler IDs
- active chains
- active virtual hosts
- active proxy/router/static capabilities
- reloadable status

The implemented phases use the existing `ReloadableModule` pattern for active
handler, proxy, router, resource, virtual-host, path-prefix service, token, and
handler-specific config files. Phase 7 exposes a `capabilities` summary from
`get_service_info`, including active modules, traffic capabilities, active
handlers, chain names, path mappings, default handlers, virtual hosts, and
path-resource config.

## Suitable First Handlers

Start with handlers that map cleanly to Pingora request and response metadata:

- correlation ID and trace headers
- response headers
- metrics
- CORS
- JWT verification
- API key verification
- basic auth
- request size limit from headers
- simple rate limiting by principal, IP, host, or route

Defer handlers that require deeper body handling:

- request decompression
- response compression policy beyond Pingora modules
- request body sanitizer
- generic body parser
- WebSocket message handlers

## Error Model

Handlers and proxy/resource selection should return structured errors that
render consistently.

```rust
pub struct HandlerError {
    pub status: u16,
    pub code: Cow<'static, str>,
    pub message: Cow<'static, str>,
    pub metadata: serde_json::Value,
}
```

Security handlers should avoid returning sensitive validation details to the
browser. Detailed diagnostics should go to logs with correlation IDs.

Common gateway errors:

- unknown host: `404`
- no matching handler path or static resource: `404`
- unsupported method for static route: `405`
- static file outside root: `403`
- missing upstream: startup validation error
- auth failure: `401` or `403`
- rate limit: `429`

## Testing Strategy

Unit tests in `light-pingora`:

- build active handler set from referenced `paths` and `defaultHandlers`
- ignore registered but unreferenced handlers
- do not require config files for unreferenced handlers
- parse valid `handler.yml`
- reject unknown handler IDs
- reject recursive chains
- resolve path/default handler chains in order
- parse `handler.yaml` fallback
- merge `additionalHandlers`, `additionalChains`, and `additionalPaths`
- capture path-template variables
- parse CORS list/string and path-prefix config
- classify metrics status codes
- normalize host names and strip ports
- reject duplicate virtual hosts
- match exact virtual hosts
- parse and validate `proxy.yml` hosts
- parse and validate `router.yml` rewrite-rule config
- select router targets from direct `service_url`
- reject direct router targets that do not match `hostWhitelist`
- select router targets from controller discovery and static `serviceTargets`
- apply router URL, method, query-parameter, and header rewrites
- parse `pathPrefixService.yml` and avoid partial-segment path matches
- parse `token.yml` and the client credentials subset of `client.yml`
- support single and multiple auth-server token configuration
- discover token service endpoints from `client.yml` token `serviceId`
- mask token cache summaries and never expose bearer token values
- expose gateway capabilities in `get_service_info`
- prevent static path traversal
- deny dotfiles by default
- serve `index.html` for `/`
- serve SPA fallback for non-asset paths
- avoid SPA fallback for `/api/` proxy routes
- select cache headers for `index.html` and hashed assets
- stop handler execution on early response
- run response handlers before static response write

Integration tests:

- same binary starts with BFF profile config
- same binary starts with proxy or balancer profile config
- BFF profile can route API paths through configured handlers and serve SPA
  fallback through `defaultHandlers`
- static SPA route returns `index.html`
- static asset route returns correct content type and cache header
- virtual host A and virtual host B serve different roots
- API route is proxied to the configured `proxy.yml` upstream
- auth handler blocks protected API routes
- public static route does not require auth unless configured

## Rollout Plan

Phase 1: Product config and active handler model (implemented)

- keep a single `apps/light-gateway` binary
- register all built-in handler descriptors explicitly
- resolve active handler IDs from `handler.yml`
- instantiate only active handlers
- load config only for active handlers
- document product profiles managed by `light-portal`

Phase 2: BFF and fixed proxy engine (implemented)

- load and register `proxy.yml`, `path-resource.yml`, and `virtual-host.yml`
- match `handler.yml` paths and fallback handlers in Java-compatible order
- select fixed proxy upstreams from `proxy.yml`
- match virtual hosts by `Host`
- serve single-site and virtual-host static content

- implement safe static path resolution
- serve static files from `request_filter`
- add Rust SPA fallback improvement
- add content type and cache headers
- add traversal, dotfile, fallback, proxy-host, and virtual-host tests

Phase 3: Handler chain execution (implemented)

- run request and response handlers around static and proxied responses
- implement correlation, CORS, and basic metrics
- parse `correlation.yml`, `cors.yml`, and `metrics.yml`
- pass generated correlation IDs upstream
- apply response headers to both static and proxied responses
- log handler duration when `reportHandlerDuration` is enabled
- defer generic response headers to a handler-specific follow-up

Phase 4: Security and request/response policy handlers (implemented)

- implement JWT, API key, basic auth, and rate-limit handlers
- implement the generic header handler for request and response mutation
- implement unified-security path-prefix selection for Basic, JWT, and API key
- parse Java-compatible `security.yml`, `apikey.yml`, `basic-auth.yml`,
  `unified-security.yml`, `header.yml`, and `limit.yml`
- add JWT pass-through claim request header mutation
- add path-level chain selection for public SPA and protected API routes

Phase 5: Sidecar router (implemented)

- load and register `router.yml`
- implement dynamic target selection by `service_url` or `service_id`
- enforce `hostWhitelist`
- support static `serviceTargets` for `service_id` routing until runtime
  discovery is available
- support router URL, method, query-parameter, and header rewrites
- apply router request mutation in `upstream_request_filter`
- remove router selection headers before forwarding
- include router config in the active reload model
- add sidecar-focused tests

Phase 6: Sidecar path-prefix and token flow (implemented)

- load and register `pathPrefixService.yml`
- resolve `service_id` by longest path-boundary prefix
- load and register `token.yml`
- load and register the token-related view of `client.yml`
- support single-auth-server and `multipleAuthServers` client credentials
- cache tokens locally and expose masked cache summaries through the runtime
  cache registry
- inject `Authorization` or `X-Scope-Token` according to inbound request state
- extend reload coverage to `pathPrefixService.yml`, `token.yml`, and
  token-related `client.yml`
- add sidecar token/path-prefix tests

Phase 7: Discovery and control plane (implemented)

- expose the runtime portal-registry client to framework transports
- add `discovery/lookup` support to the portal-registry client
- resolve router `service_id` targets through controller discovery
- keep static `router.serviceTargets` as a fallback for local/static profiles
- discover token service endpoints from `client.yml` token `serviceId`
- expose active capabilities, hosts, paths, handlers, and chains through
  `get_service_info`
- atomically replace resolved handler/resource/proxy models on reload

Phase 8: Advanced transport features (implemented)

- add streaming static-file delivery for files at or above `transferMinSize`
- add conditional static requests with `ETag` and `Last-Modified`
- add wildcard virtual hosts with exact-host precedence
- evaluate multi-cert TLS SNI support and document the Rustls limitation

## Phase 2 Decisions

- Static roots can be absolute, matching the Java deployment model, or relative
  to the runtime config directory for local Rust development.
- SPA fallback applies only to browser routes. Paths that look like assets,
  such as `/app.js` or `/favicon.ico`, return 404 when the file is missing.
- Handler path matching supports exact paths and Java/OpenAPI-style `{name}`
  path-template segments.

## Open Questions

- Should static content support `ETag` in the first implementation if portal
  deployments depend on browser cache validation?
