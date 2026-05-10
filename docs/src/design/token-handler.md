# Token Handler

## Status

Proposed design for migrating the Java `egress-router` `TokenHandler` into
`light-fabric` as the `token` handler used by `light-pingora` and
`light-gateway`.

A baseline Rust token runtime already exists in `light-pingora`. This document
captures the Java behavior, the compatibility contract, and the design direction
for hardening it for gateway and sidecar deployments.

## Purpose

The token handler obtains an OAuth 2.0 client credentials access token on behalf
of the backend service in the sidecar or gateway egress path. The token is then
attached to the outbound request before `router` or `proxy` sends the request to
the downstream API.

This is different from the PII `tokenize` and `detokenize` handlers. The
`token` handler deals only with service-to-service OAuth tokens.

## Java Behavior To Map

The Java implementation is centered on:

- `egress-router/.../TokenHandler.java`
- `sidecar/.../SidecarTokenHandler.java`
- `router-config/.../TokenConfig.java`
- `client-config/.../client.yaml`
- `sidecar-config/.../sidecar.yml`

`token.yml` controls whether the handler is active and which request paths need
token injection:

```yaml
enabled: ${token.enabled:false}
appliedPathPrefixes: ${token.appliedPathPrefixes:}
```

The OAuth provider, client credentials, cache, timeout, proxy, HTTP/2, and
single-vs-multiple-auth-server settings live in `client.yml`:

```yaml
oauth:
  multipleAuthServers: ${client.multipleAuthServers:false}
  token:
    cache:
      capacity: ${client.tokenCacheCapacity:200}
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    expiredRefreshRetryDelay: ${client.expiredRefreshRetryDelay:2000}
    earlyRefreshRetryDelay: ${client.earlyRefreshRetryDelay:30000}
    server_url: ${client.tokenServerUrl:}
    serviceId: ${client.tokenServiceId:}
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
  timeout: ${client.timeout:4000}
```

The Java request flow is:

1. Reload `token.yml` for the request.
2. Check `appliedPathPrefixes` with a string prefix match.
3. Read `service_id` from the request. This header is expected to be set by
   `PathPrefixServiceHandler` or `ServiceDictHandler`.
4. Resolve the auth server configuration from `client.yml`.
5. Get or refresh a cached client credentials JWT for the service.
6. If the request has no `Authorization` header, set
   `Authorization: Bearer <token>`.
7. If the request already has `Authorization`, preserve it and set
   `X-Scope-Token: Bearer <token>`.
8. Continue to the next handler, usually `router`.

For multiple auth servers, Java reads
`oauth.token.client_credentials.serviceIdAuthServers[service_id]` and enriches
that entry with the global token defaults. For a single auth server, it uses the
global `oauth.token.client_credentials` section.

The Java cache is a static map keyed by `service_id`. The cached `Jwt` stores
the access token and its `exp` claim in milliseconds. `OauthHelper` refreshes
synchronously after expiry and attempts async refresh while the token is in the
renewal window.

`SidecarTokenHandler` adds an egress gate before calling `TokenHandler`:

- `sidecar.egressIngressIndicator: header` runs the token handler only when the
  request has `service_id` or `service_url`.
- `sidecar.egressIngressIndicator: protocol` runs the token handler for HTTP
  requests, which is the usual in-pod sidecar egress protocol.
- Any other value skips token injection.

The base Java `TokenHandler` still needs `service_id` to choose the service
token. A request with only `service_url` can identify egress traffic, but it
does not by itself select a service-specific token.

## Goals

- Preserve the Java configuration files: `token.yml` and `client.yml`.
- Activate the handler with the existing `token` id in `handler.yml`.
- Support config-server injection for `token.enabled`,
  `token.appliedPathPrefixes`, `client.multipleAuthServers`,
  `client.tokenCcServiceIdAuthServers`, `sidecar.egressIngressIndicator`, and
  the rest of the `client.yml` token fields.
- Support single auth server and per-service auth server configurations.
- Support token endpoint discovery through `oauth.token.serviceId` when a
  direct `server_url` is not configured.
- Preserve the Java header behavior for `Authorization` and `X-Scope-Token`.
- Keep token retrieval fast and safe for request-path execution.
- Register configuration and token cache state with the module registry and
  runtime cache registry without exposing token or secret values.
- Keep the design usable by `light-gateway`, future sidecar products, and BFF
  deployments that need to call downstream APIs.

## Non-Goals

- Do not use `inventory` or dynamic plugins. Handler availability is compiled
  into the binary; handler activation is controlled by `handler.yml`.
- Do not implement authorization code, refresh token, or token exchange in this
  handler. This handler only performs `client_credentials`.
- Do not migrate Java `SAMLTokenHandler` as part of this design.
- Do not use the PII tokenization table or handlers. `token`, `tokenize`, and
  `detokenize` are separate concerns.
- Do not send the generated access token to logs, metrics labels, module
  registry output, or cache summaries.

## Resolved Decisions

- Use `sidecar.yml` to differentiate inbound proxy traffic from outbound router
  traffic before applying token injection.
- Implement refresh with the same concurrency model as Java `http-client`:
  synchronize refresh per cached token, refresh expired tokens synchronously,
  refresh valid tokens in the renewal window asynchronously, and use retry
  windows to prevent repeated failed refresh attempts.

## Handler Chain

The token handler must run after service resolution and before egress routing:

```yaml
handlers:
  - correlation
  - security
  - path-prefix-service
  - token
  - router

chains:
  sidecar-egress:
    - correlation
    - security
    - path-prefix-service
    - token
    - router

paths:
  - path: /v1/pets
    method: GET
    exec:
      - sidecar-egress
```

`path-prefix-service` sets `service_id` from path configuration. `token` uses
that service id to resolve and cache the client credentials token. `router`
uses the same service id to select the downstream API target and should remove
routing-only headers before forwarding.

For products where only some outbound APIs need a scope token, keep one chain
with `token` and another without it, or use `token.appliedPathPrefixes` to
limit token injection inside a shared chain.

## Rust Architecture

Keep the implementation in `light-pingora` because token injection is a
request-path gateway handler. `light-gateway` wires the handler into the
existing chain execution model.

Primary Rust module:

```text
frameworks/light-pingora/src/token.rs
```

Primary types:

```rust
pub struct TokenHandlerConfig {
    pub enabled: bool,
    pub applied_path_prefixes: Vec<String>,
}

pub struct ClientTokenConfig {
    pub tls: ClientTlsConfig,
    pub oauth: ClientOauthConfig,
    pub path_prefix_services: BTreeMap<String, String>,
    pub request: ClientRequestConfig,
}

pub struct TokenRuntime {
    handler: TokenHandlerConfig,
    sidecar: SidecarTrafficConfig,
    client: ClientTokenConfig,
    cache: Arc<TokenCache>,
    registry_client: Option<Arc<PortalRegistryClient>>,
}
```

`apps/light-gateway` should load `TokenRuntime` only when the matched handler
configuration contains `token`. For Java compatibility, `token.yml` still has
`enabled`; therefore the handler is effective only when both conditions are
true:

```text
handler.yml contains token
token.yml enabled is true
```

If `token.yml` enables the handler, `client.yml` is required and invalid
configuration should fail startup. `sidecar.yml` is also loaded into the token
runtime so the same handler chain can distinguish inbound proxy requests from
outbound router requests. Invalid reloads should be rejected while the last
valid runtime keeps serving traffic.

## Request Flow

The Rust request flow should be:

1. Resolve the active handler chain for the path and method.
2. When `token` is encountered, check `TokenHandlerConfig.enabled`.
3. Evaluate `sidecar.yml` and skip token injection for inbound proxy traffic.
4. Check `appliedPathPrefixes` with boundary-aware matching. `/v1/address`
   should match `/v1/address/123`, but not `/v1/address2`.
5. Resolve the token service id:
   - first from the `service_id` request header,
   - then from `client.yml pathPrefixServices`,
   - then from `oauth.token.serviceId` for single-auth-server token endpoint
     discovery when applicable.
6. Resolve the token endpoint:
   - use direct `server_url` first,
   - otherwise discover `oauth.token.serviceId` through portal registry.
7. Select client credentials:
   - for single auth server, use `oauth.token.client_credentials`,
   - for multiple auth servers, require
     `client_credentials.serviceIdAuthServers[service_id]` and merge it with
     global token defaults.
8. Look up the token cache.
9. Fetch a new token when the cache is missing, expired, or inside the refresh
   window.
10. Add `Authorization` or `X-Scope-Token` using the Java-compatible rule.

The outbound token request should be Java-compatible:

```http
POST {server_url}{uri}
Content-Type: application/x-www-form-urlencoded
Accept: application/json
Authorization: Basic base64(client_id:client_secret)

grant_type=client_credentials&scope=...
```

The response must contain `access_token`. Expiry should be derived from the JWT
`exp` claim when available, with `expires_in` as a fallback for non-JWT token
servers.

## Cache And Refresh

Use a bounded async cache owned by `TokenRuntime`.

The cache key should include both service id and scope:

```rust
pub struct TokenCacheKey {
    pub service_id: Option<String>,
    pub scope: Option<String>,
}
```

This is stricter than the Java `Map<String, Jwt>` keyed only by `service_id`
and avoids collisions when the same service uses multiple scope sets.

Refresh policy:

- If the token is valid and outside the renewal window, use the cached token.
- If the token is expired, synchronize on that cache entry and refresh
  synchronously. Concurrent requests for the same service and scope should wait
  on the same per-entry lock, then re-check the refreshed token instead of
  making duplicate token endpoint calls.
- If the token is expired but another failed refresh attempt is still inside
  `expiredRefreshRetryDelay`, fail closed with a token-not-available rejection.
- If the token is in the renewal window but not expired, return the current
  token and start one background refresh for that cache entry when no refresh is
  already running and `earlyRefreshRetryDelay` has elapsed.
- Keep refresh state per cached token: token string, expiry, scope, `renewing`,
  `expired_retry_timeout`, and `early_retry_timeout`.

This intentionally mirrors Java `OauthHelper.populateCCToken`. The Rust
implementation should use `tokio` locks/tasks instead of Java `synchronized`
and `ScheduledExecutorService`, but the observable behavior should stay the
same: expired tokens block the current request, early refresh does not block the
current request, and multiple concurrent requests for the same token are
coordinated through one cache entry.

On `token.yml` or `client.yml` reload, build a new `TokenRuntime` and discard
the old cache. This prevents tokens issued with old client credentials or old
scope configuration from being reused after a config change.

## Sidecar Egress Gate

The token handler must use `sidecar.yml` to decide whether the current request
is outbound router traffic or inbound proxy traffic. This allows one gateway or
sidecar process to host both directions while applying token injection only to
egress calls.

Use the Java `sidecar.yml` contract:

```yaml
egressIngressIndicator: ${sidecar.egressIngressIndicator:header}
```

Rust behavior:

- `header`: run `token` only when `service_id` or `service_url` is present.
- `protocol`: run `token` for HTTP requests entering the sidecar listener.
- any other value: skip token injection.

Even with this gate, token selection should still require either a resolved
service id or a single-auth-server configuration that can use a direct
`server_url`.

The sidecar config should be registered in the module registry as a framework
config. Invalid values should fail startup or reject reload.

## Configuration Examples

Single auth server:

```yaml
# sidecar.yml
egressIngressIndicator: ${sidecar.egressIngressIndicator:header}
```

```yaml
# token.yml
enabled: ${token.enabled:true}
appliedPathPrefixes: ${token.appliedPathPrefixes:/v1}
```

```yaml
# client.yml
oauth:
  multipleAuthServers: false
  token:
    server_url: ${client.tokenServerUrl:https://oauth.example.com}
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    client_credentials:
      uri: ${client.tokenCcUri:/oauth2/token}
      client_id: ${client.tokenCcClientId:gateway-client}
      client_secret: ${client.tokenCcClientSecret:}
      scope: ${client.tokenCcScope:petstore.r petstore.w}
```

Multiple auth servers:

```yaml
# client.yml
oauth:
  multipleAuthServers: true
  token:
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    client_credentials:
      uri: /oauth2/token
      serviceIdAuthServers: ${client.tokenCcServiceIdAuthServers:}
pathPrefixServices: ${client.pathPrefixServices:}
```

The config server can inject `client.tokenCcServiceIdAuthServers` as YAML or a
JSON string:

```yaml
com.networknt.petstore-1.0.0:
  server_url: https://oauth-petstore.example.com
  client_id: petstore-client
  client_secret: ${PETSTORE_CLIENT_SECRET}
  scope:
    - petstore.r
    - petstore.w
```

## Rust Improvements Over Java

- Use boundary-aware path prefix matching instead of raw `startsWith`.
- Include scope in the cache key.
- Mask `client_secret` and token values in module registry and cache output.
- Fail startup for enabled but invalid token configuration.
- Use Rust async primitives to implement the same per-token synchronized refresh
  behavior as Java without spawning a dedicated executor per refresh attempt.
- Support direct `server_url` and portal-registry discovery with the same
  runtime path.
- Keep all config-server injected values in the normal module registry and
  reload model.

## Observability

Record metrics and logs around the token operation, but never include the token
or client secret:

- handler duration for `token`,
- cache hit, miss, refresh, and failure counts,
- token endpoint latency and HTTP status,
- service id and provider selection,
- refresh retry suppression counts,
- module registry entry for loaded `token.yml` and masked `client.yml`,
- runtime cache entry count and expiry summaries without access token strings.

## Failure Behavior

Fail closed when token injection is required but cannot be completed:

- missing `service_id` for multiple auth servers,
- missing `serviceIdAuthServers[service_id]`,
- missing `client_id` or `client_secret`,
- no direct `server_url` and failed token service discovery,
- token endpoint returns non-2xx,
- token response has no `access_token`,
- token response has neither JWT `exp` nor `expires_in`,
- invalid proxy, URL, or TLS configuration.

Requests outside `appliedPathPrefixes` should bypass the handler without error.

## Test Plan

Unit tests in `light-pingora`:

- parse Java-compatible `token.yml` and `client.yml`,
- parse and validate Java-compatible `sidecar.yml`,
- parse `appliedPathPrefixes` as YAML list, JSON string list, and comma list,
- parse `serviceIdAuthServers` as YAML map and JSON string map,
- verify boundary-aware prefix matching,
- verify `sidecar.yml` header mode applies token only to outbound requests with
  `service_id` or `service_url`,
- verify `sidecar.yml` protocol mode applies token to HTTP egress traffic,
- verify single auth server option resolution,
- verify multiple auth server option merging,
- verify `Authorization` versus `X-Scope-Token` header selection,
- verify cache key includes service id and scope,
- verify token cache summaries never include token strings,
- verify expired token refresh is synchronized across concurrent requests,
- verify early-window refresh returns the current token and starts only one
  background refresh.

Gateway tests in `light-gateway`:

- chain with `path-prefix-service -> token -> router`,
- inbound proxy request skips token injection according to `sidecar.yml`,
- outbound router request applies token injection according to `sidecar.yml`,
- missing service id for multiple auth servers returns a handler rejection,
- existing caller `Authorization` is preserved and scope token is added to
  `X-Scope-Token`,
- token runtime reload swaps config and clears old cache,
- inactive `token` handler does not require `token.yml` or `client.yml`.

Integration tests:

- mock OAuth token endpoint with client credentials Basic auth,
- mock discovered token service through portal registry,
- mock downstream service and assert the final outbound headers,
- refresh behavior with expired and near-expiry tokens.
