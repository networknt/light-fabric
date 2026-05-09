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
with different `handler.yml`, route config, and handler-specific config files.

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
- Integrate loaded handler and gateway config with `ModuleRegistry`.
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
  -> normalize Host header
  -> match virtual host
  -> match route by path and method
  -> select handler chain from handler.yml paths/defaultHandlers
  -> run request handlers
  -> serve static file, or proxy upstream, or return error
  -> run response handlers
  -> response
```

For static routes, `request_filter` writes the response and returns `Ok(true)`
so Pingora does not proxy the request.

For proxy routes, `request_filter` stores the selected route and upstream in
the per-request context and returns `Ok(false)`. `upstream_peer` and
`upstream_request_filter` then use that context to connect to the right
upstream and set headers.

## Crate Layout

Keep the first implementation inside `frameworks/light-pingora`.

Suggested modules:

```text
frameworks/light-pingora/src/
  lib.rs
  handler.rs
  handler_config.rs
  gateway.rs
  static_files.rs
  virtual_host.rs
```

Responsibilities:

- parse and validate `handler.yml`
- parse and validate gateway route config
- build explicit handler registry
- resolve handler chains
- match virtual hosts and routes
- serve static SPA content
- wrap application `ProxyHttp` implementations when needed
- expose module-registry entries for active handler and gateway config

This keeps the first implementation close to the Pingora lifecycle and avoids
premature abstractions for Axum.

If Axum later needs the same handler semantics, extract the framework-neutral
parts after the Pingora implementation has stabilized.

## Configuration Split

Use `handler.yml` for the Java-compatible handler middleware contract:
handler declarations, reusable chains, path-to-chain mappings, and fallback
handlers.

Use `gateway.yml` for product mode, virtual hosts, static roots, routes,
upstreams, and gateway-family behavior. Keeping one route config name makes
config-server delivery and product templates easier to reason about.

Handler-specific files such as `cors.yml`, `jwt.yml`, `rate-limit.yml`, and
`headers.yml` stay separate. They are loaded only when the corresponding
handler is active in the resolved path/default execution model.

### Remote Config Source

`light-gateway` starts with enough local bootstrap configuration to contact
config-server. The existing Light Fabric runtime then resolves local and remote
configuration before `light-pingora` builds the route set.

Startup flow:

1. load local bootstrap files from the configured config directory
2. contact config-server using the configured service identity, environment,
   and authorization
3. download remote product configuration managed by `light-portal`
4. merge remote config with local fallback config
5. load `handler.yml`, `gateway.yml`, and active handler-specific config files
6. validate the complete route and handler model
7. bind Pingora listeners
8. register the runtime instance with the controller

The remote product config should include:

- `handler.yml`
- `gateway.yml`
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

### Gateway Config

Example `gateway.yml` for a BFF profile:

```yaml
enabled: true
mode: bff

virtualHosts:
  - host: admin.example.com
    static:
      root: /opt/light/spa/admin
      index: /index.html
      spaFallback: true
    routes:
      - pathPrefix: /api/
        upstream: admin-api
      - pathPrefix: /oauth/
        upstream: oauth
      - pathPrefix: /
        static: true

  - host: portal.example.com
    static:
      root: /opt/light/spa/portal
      index: /index.html
      spaFallback: true
    routes:
      - pathPrefix: /api/
        upstream: portal-api
      - pathPrefix: /
        static: true

upstreams:
  admin-api:
    address: admin-api:8443
    tls: true
    sni: admin-api
    hostHeader: admin-api
  portal-api:
    address: portal-api:8443
    tls: true
    sni: portal-api
    hostHeader: portal-api
  oauth:
    address: light-oauth:8443
    tls: true
    sni: light-oauth
    hostHeader: light-oauth
```

`handler.yml` owns middleware chain selection through `paths` and
`defaultHandlers`. `gateway.yml` owns traffic routing: virtual hosts, static
roots, proxy route prefixes, and upstreams. Keeping these concerns separate
preserves the Java handler contract without making gateway routing depend on
Java handler class names or aliases.

Other product personas use the same config file with different sections and
chains. For example, a sidecar profile may have no static block and may use an
outbound chain for token/header propagation. A balancer profile may mostly
define upstream pools, health checks, and a small metrics/correlation chain.

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
    pub route: Option<Arc<RouteRule>>,
    pub chain: Option<Arc<ResolvedHandlerChain>>,
    pub upstream: Option<Arc<Upstream>>,
    pub attributes: BTreeMap<String, serde_json::Value>,
}
```

The context is created by `ProxyHttp::new_ctx()` and populated in
`request_filter`.

`upstream_peer` should only select an upstream after `request_filter` has
matched a proxy route. If no upstream is selected, the implementation should
return a clear configuration error rather than silently falling back.

## Virtual Hosts

Virtual-host routing should use the HTTP `Host` header.

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

Static SPA rendering should be part of the Pingora route engine, not a generic
middleware handler. It is enabled by product config, typically for `mode: bff`.

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

## Proxy Routes

Proxy routes should select an upstream by route.

Matching order:

1. virtual host
2. route path prefix or exact path
3. method, if configured
4. first matching route wins

The route should store the selected upstream in `GatewayRequestContext`.

`upstream_peer` creates the `HttpPeer` from that upstream:

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

Startup should validate handler and gateway configuration before binding
listeners.

Validation rules:

- every handler ID in `handler.yml` must exist in the explicit registry
- every chain item must resolve to a registered handler or another chain
- recursive chain references are invalid
- every route chain must exist
- every route upstream must exist unless the route is static
- every static virtual host must have a static root
- static roots must be absolute or resolved relative to a configured base
- duplicate exact virtual hosts are invalid
- duplicate handler IDs in the registry are invalid

The resolved model should be immutable and cheap to read:

```rust
pub struct GatewayRouteSet {
    pub virtual_hosts: BTreeMap<String, Arc<VirtualHost>>,
    pub default_host: Option<Arc<VirtualHost>>,
    pub chains: BTreeMap<String, Arc<ResolvedHandlerChain>>,
    pub upstreams: BTreeMap<String, Arc<Upstream>>,
}
```

Later config reload can swap the whole route set atomically.

## Runtime Integration

`light-runtime` remains responsible for bootstrap, config loading, lifecycle,
controller registration, and module registry. `light-pingora` should load its
Pingora-specific handler and gateway config through the existing runtime config
loader.

Suggested module IDs:

- `light-pingora/handler`
- `light-pingora/gateway`
- `light-pingora/correlation`
- `light-pingora/cors`
- `light-pingora/jwt`

The module registry should expose:

- handler config snapshot, masked
- gateway product mode, virtual host, route, and upstream config snapshot,
  masked
- active handler IDs
- active chains
- active virtual hosts
- active product mode
- reloadable status

If reload is added, use the existing `ReloadableModule` pattern and replace the
resolved route set atomically. In-flight requests should keep using the route
set they already selected.

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

Handlers and route selection should return structured errors that render
consistently.

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
- no matching route: `404`
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
- resolve route chains in order
- normalize host names and strip ports
- reject duplicate virtual hosts
- match exact virtual hosts
- match route prefixes in configured order
- reject route upstream references that do not exist
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
- static SPA route returns `index.html`
- static asset route returns correct content type and cache header
- virtual host A and virtual host B serve different roots
- API route is proxied to the configured upstream
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

Phase 2: Pingora gateway route engine

- add gateway config model in `light-pingora`
- support product mode in `gateway.yml`
- match virtual hosts by `Host`
- match static and proxy routes
- validate route and upstream references
- register gateway config with `ModuleRegistry`

Phase 3: Static SPA serving

- implement safe static path resolution
- serve static files from `request_filter`
- add SPA fallback
- add content type and cache headers
- add traversal, dotfile, and fallback tests

Phase 4: Handler chain

- parse `handler.yml`
- add explicit `PingoraHandlerRegistry`
- resolve chains
- run request and response handlers around static and proxied responses
- implement correlation, headers, CORS, and metrics

Phase 5: Security and upstream behavior

- implement JWT, API key, basic auth, and rate-limit handlers
- add upstream header mutation
- add route-level chain selection for public SPA and protected API routes

Phase 6: Reload and control plane

- make gateway and handler config reloadable
- expose active product mode, hosts, routes, handlers, and chains through
  service-info MCP
- atomically replace resolved route sets on reload

Phase 7: Advanced transport features

- add streaming static-file delivery if large assets require it
- add conditional requests with `ETag` or `Last-Modified`
- add wildcard virtual hosts
- evaluate multi-cert TLS SNI support

## Open Questions

- Should product mode be a field in `gateway.yml`, inferred from the
  light-portal product record, or both?
- Should static roots be absolute only, or resolved relative to the runtime
  config directory?
- Should SPA fallback be disabled automatically for paths that look like file
  assets, such as `/app.js` or `/favicon.ico`?
- Should route matching support path templates in phase 1, or only exact path
  and prefix rules?
- Should static content support `ETag` in the first implementation if portal
  deployments depend on browser cache validation?
