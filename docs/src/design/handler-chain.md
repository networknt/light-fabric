# Handler Chain

Status: Phase 1 implemented; later phases proposed

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
- Keep the design compatible with future config reload.

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
  handler_config.rs
  proxy.rs
  router.rs
  path_resource.rs
  static_files.rs
  virtual_host.rs
```

Responsibilities:

- parse and validate `handler.yml`
- parse and validate `proxy.yml`, `router.yml`, `path-resource.yml`, and
  `virtual-host.yml`
- build explicit handler registry
- resolve handler chains
- match handler paths and fallback handlers
- serve static SPA content
- select fixed proxy upstreams from `proxy.yml`
- select dynamic sidecar/router upstreams from `router.yml` and discovery
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

Handler-specific files such as `cors.yml`, `jwt.yml`, `rate-limit.yml`, and
`headers.yml` stay separate. They are loaded only when the corresponding
handler is active in the resolved path/default execution model.

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

Unlike Java, the Rust `handlers` list uses stable short handler IDs. It does
not use fully qualified class names, and it does not need `@alias` because the
IDs are already short and stable.

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
discovery. Rust should follow the same model when router support is added.
Because this depends on sidecar token, cache, and discovery behavior, full
router execution should be implemented after the fixed proxy and static BFF
path are stable.

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

```rust
#[async_trait::async_trait]
pub trait PingoraHandler: Send + Sync {
    fn id(&self) -> &'static str;

    async fn on_request(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
    ) -> Result<HandlerDecision>;

    async fn on_upstream_request(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut GatewayRequestContext,
    ) -> Result<()>;

    async fn on_response(
        &self,
        session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut GatewayRequestContext,
    ) -> Result<()>;
}
```

Request handlers return:

```rust
pub enum HandlerDecision {
    Continue,
    Respond(HandlerResponse),
}
```

If a handler returns `Respond`, `light-gateway` writes that response and stops
the chain. This is how auth failures, rate-limit failures, and request
validation errors short-circuit the proxy.

Response handlers should run before both static and proxied responses are sent.
For proxied responses, this maps to Pingora `response_filter`. For static
responses, the static-file renderer calls the same response handler chain
before writing the local response.

## Request Context

The per-request context should carry route decisions across Pingora phases.

```rust
pub struct GatewayRequestContext {
    pub host: Option<Arc<VirtualHost>>,
    pub path: Option<Arc<ResolvedHandlerPath>>,
    pub chain: Option<Arc<ResolvedHandlerChain>>,
    pub upstream: Option<Arc<SelectedUpstream>>,
    pub attributes: BTreeMap<String, serde_json::Value>,
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
- optional wildcard match such as `*.example.com` later

HTTP host routing is enough for the first implementation.

TLS certificate selection by SNI is separate. The current `light-pingora`
transport uses one Rustls TLS setting for the listener, so the first production
options are:

- terminate TLS at ingress or a load balancer
- use a wildcard certificate
- use one certificate with all required SANs

Dynamic multi-cert SNI selection can be added later as a transport enhancement.

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

The first implementation can read a static file into memory and write it with
`Session::write_response_header` and `Session::write_response_body`. If large
SPA assets become a problem, add streaming file delivery as a focused follow-up.

Conditional requests with `ETag` or `Last-Modified` are useful but can be a
second phase. They are not required to make SPA serving work.

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

`router.yml` selects from request metadata and discovery. This should be a
separate follow-up after proxy/static support because the useful sidecar path
also needs token acquisition, token cache, path-prefix service mapping, direct
registry or discovery, and host whitelist behavior.

Router target behavior:

- prefer `service_url` when present and allowed by `router.hostWhitelist`
- otherwise use `service_id` plus optional `env_tag`
- optionally allow `service_id` from the query string when
  `serviceIdQueryParameter` is true
- discover targets through the configured registry/cluster
- support URL, method, query-parameter, and header rewrite rules

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

Later config reload can swap the whole runtime model atomically.

## Runtime Integration

`light-runtime` remains responsible for bootstrap, config loading, lifecycle,
controller registration, and module registry. `light-pingora` should load its
Pingora-specific handler, traffic, and resource config through the existing
runtime config loader.

Suggested module IDs:

- `light-pingora/handler`
- `light-pingora/proxy`
- `light-pingora/router`
- `light-pingora/path-resource`
- `light-pingora/virtual-host`
- `light-pingora/correlation`
- `light-pingora/cors`
- `light-pingora/jwt`

The module registry should expose:

- handler config snapshot, masked
- proxy, router, path-resource, and virtual-host config snapshots, masked
- active handler IDs
- active chains
- active virtual hosts
- active proxy/router/static capabilities
- reloadable status

If reload is added, use the existing `ReloadableModule` pattern and replace the
resolved runtime model atomically. In-flight requests should keep using the
handler/resource/proxy model they already selected.

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
- normalize host names and strip ports
- reject duplicate virtual hosts
- match exact virtual hosts
- parse and validate `proxy.yml` hosts
- parse and validate `router.yml` rewrite-rule config
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

Phase 1: Product config and active handler model

- keep a single `apps/light-gateway` binary
- register all built-in handler descriptors explicitly
- resolve active handler IDs from `handler.yml`
- instantiate only active handlers
- load config only for active handlers
- document product profiles managed by `light-portal`

Phase 2: BFF and fixed proxy engine

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

Phase 3: Handler chain execution

- run request and response handlers around static and proxied responses
- implement correlation, headers, CORS, and metrics

Phase 4: Security and upstream behavior

- implement JWT, API key, basic auth, and rate-limit handlers
- add upstream header mutation
- add path-level chain selection for public SPA and protected API routes

Phase 5: Sidecar router

- load and register `router.yml`
- implement dynamic target selection by `service_url` or `service_id`
- enforce `hostWhitelist`
- integrate path-prefix service mapping, token/cache behavior, and discovery
- support router URL, method, query-parameter, and header rewrites
- add sidecar-focused tests

Phase 6: Reload and control plane

- add router-specific reload once `router.yml` is implemented
- extend reload coverage to security and traffic handler configs
- expose active capabilities, hosts, paths, handlers, and chains through
  service-info MCP
- atomically replace resolved handler/resource/proxy models on reload

Phase 7: Advanced transport features

- add streaming static-file delivery if large assets require it
- add conditional requests with `ETag` or `Last-Modified`
- add wildcard virtual hosts
- evaluate multi-cert TLS SNI support

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
