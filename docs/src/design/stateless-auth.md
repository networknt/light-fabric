# Stateless Auth Handler

## Status

Initial Rust implementation is complete in `light-pingora` and
`light-gateway`. It includes the shared SPA session runtime, authorization-code
entrypoint, logout, cookie handling, CSRF validation, refresh-token renewal,
Google/Facebook/GitHub callback entrypoints, handler wiring, config stubs, and
runtime-load tests.

## Purpose

The Java `light-spa-4j` `stateless-auth` module is the BFF login bridge for
SPA deployments that use OAuth 2.0 authorization code flow in the cloud. The
browser completes the provider redirect, calls the gateway callback path with
the authorization code, and the gateway exchanges that code for light-oauth
tokens. The gateway then stores the internal access token, refresh token, user
metadata, and CSRF value in browser cookies.

In light-fabric this should be a `light-pingora` security handler used by
`light-gateway`. The handler should be activated by `handler.yml`, loaded from
config-server with the same product-level configuration model as the rest of
the gateway, and implemented with the same shared SPA session runtime used by
the MSAL exchange handler.

## Goals

- Preserve the Java BFF behavior for authorization code login, logout, CSRF
  validation, refresh-token renewal, and downstream `Authorization` injection.
- Keep the Java `statelessAuth.yml` field names recognizable so light-portal
  can inject `statelessAuth.*` values into config-server output.
- Use `handler.yml` as the primary activation and ordering contract.
- Keep the existing `stateless` handler id as the public handler-chain name.
- Share cookie, CSRF, JWT parsing, refresh-token single-flight, and
  `Authorization` injection code with the MSAL exchange handler.
- Use the existing `client.yml` OAuth token configuration for authorization
  code and refresh-token calls.
- Register the loaded config in `ModuleRegistry` and reject invalid config at
  startup.
- Support BFF chains that also use static SPA serving, proxy/router,
  WebSocket routing, and MCP routing.
- Support Google, Facebook, and GitHub login entrypoints in addition to the
  generic authorization-code callback.

## Non-Goals

- Do not use Rust dynamic plugins or `inventory`.
- Do not create a separate BFF binary.
- Do not store server-side browser sessions in the first implementation.
- Do not require the Rust social-login implementation to copy Java's
  provider-specific classes. Rust should preserve the external behavior and
  config contract, but it can use established OAuth/OIDC crates for provider
  protocol handling.
- Do not redirect the browser from the gateway by default. Java returns a JSON
  body containing `redirectUri`, `denyUri`, and `scopes`; Rust should preserve
  that behavior.

## Resolved Decisions

- Google, Facebook, and GitHub login handlers are in scope. The existing
  `google`, `facebook`, and `github` handler ids should remain as public
  handler-chain names.
- Rust should prefer provider-appropriate crates instead of hand-rolling every
  provider flow. `openidconnect` is a good fit for OpenID Connect providers
  such as Google, and `oauth2` is a good fit for plain OAuth 2.0 providers or
  provider-specific extensions.
- `cookieTimeoutUri` should be used by Rust to return a structured
  session-expired response when a browser session cannot be renewed.

## Java Behavior To Map

Java config file:

```yaml
enabled: ${statelessAuth.enabled:true}
redirectUri: ${statelessAuth.redirectUri:https://localhost:3000/#/app/dashboard}
denyUri: ${statelessAuth.denyUri:https://localhost:3000/#/app/dashboard}
enableHttp2: ${statelessAuth.enableHttp2:false}
authPath: ${statelessAuth.authPath:/authorization}
logoutPath: ${statelessAuth.logoutPath:/logout}
cookieDomain: ${statelessAuth.cookieDomain:localhost}
cookiePath: ${statelessAuth.cookiePath:/}
cookieTimeoutUri: ${statelessAuth.cookieTimeoutUri:/}
cookieSecure: ${statelessAuth.cookieSecure:true}
sessionTimeout: ${statelessAuth.sessionTimeout:3600}
rememberMeTimeout: ${statelessAuth.rememberMeTimeout:604800}
bootstrapToken: ${statelessAuth.bootstrapToken:token}
googlePath: ${statelessAuth.googlePath:/google}
googleClientId: ${statelessAuth.googleClientId:google_client_id}
googleClientSecret: ${statelessAuth.googleClientSecret:secret}
googleRedirectUri: ${statelessAuth.googleRedirectUri:https://localhost:3000}
facebookPath: ${statelessAuth.facebookPath:/facebook}
facebookClientId: ${statelessAuth.facebookClientId:facebook_client_id}
facebookClientSecret: ${statelessAuth.facebookClientSecret:secret}
githubPath: ${statelessAuth.githubPath:/github}
githubClientId: ${statelessAuth.githubClientId:github_client_id}
githubClientSecret: ${statelessAuth.githubClientSecret:secret}
```

Java request behavior:

- `GET authPath`, normally `/authorization`, expects query parameter `code`
  and optional `state`.
- Missing `code` returns `ERR10035`.
- The handler generates a CSRF value and sends an authorization-code token
  request through `http-client` using `client.yml`
  `oauth.token.authorization_code`.
- On success, it sets browser cookies and returns JSON containing `scopes`,
  `redirectUri`, and `denyUri`.
- `GET logoutPath`, normally `/logout`, clears BFF cookies and ends the
  request.
- Other requests are treated as downstream BFF requests. The handler reads the
  `accessToken` cookie, verifies/parses it, validates CSRF, refreshes the token
  if it expires within 90 seconds, and injects
  `Authorization: Bearer <access-token>` before the proxy/router handler runs.
- If no access token exists but a refresh token exists, the handler attempts
  refresh and then injects the new access token.
- If neither cookie exists, Java allows the request to continue. The downstream
  service can still decide whether the endpoint is anonymous or protected.

Java error codes to preserve:

| Code | Meaning |
| --- | --- |
| `ERR10035` | Authorization code is missing |
| `ERR10000` | Access token is invalid |
| `ERR10036` | CSRF token is missing from request |
| `ERR10038` | CSRF claim is missing from JWT |
| `ERR10039` | Request CSRF and JWT CSRF do not match |
| `ERR10037` | Refresh-token response is empty |

## Rust Architecture

Add a shared SPA auth runtime in `light-pingora` and expose it through
`light-gateway`.

Proposed modules:

```text
frameworks/light-pingora/src/spa_auth.rs
frameworks/light-pingora/src/stateless_auth.rs
```

`spa_auth.rs` owns the reusable mechanics:

```rust
pub struct SpaCookieConfig {
    pub cookie_domain: String,
    pub cookie_path: String,
    pub cookie_secure: bool,
    pub session_timeout: u64,
    pub remember_me_timeout: u64,
    pub same_site: CookieSameSite,
    pub renew_before_seconds: u64,
}

pub struct SpaSessionRuntime {
    pub cookies: SpaCookieConfig,
    pub token_client: Arc<SpaTokenClient>,
    pub jwt_verifier: Arc<SecurityRuntime>,
    pub refresh_single_flight: RefreshSingleFlight,
}

pub struct SpaSessionResult {
    pub access_token: Option<String>,
    pub principal: Option<AuthPrincipal>,
    pub response_cookies: Vec<SetCookie>,
}
```

`stateless_auth.rs` owns the authorization-code entrypoint:

```rust
pub struct StatelessAuthConfig {
    pub enabled: bool,
    pub redirect_uri: String,
    pub deny_uri: Option<String>,
    pub enable_http2: bool,
    pub auth_path: String,
    pub logout_path: String,
    pub cookie_domain: String,
    pub cookie_path: String,
    pub cookie_timeout_uri: String,
    pub cookie_secure: bool,
    pub session_timeout: u64,
    pub remember_me_timeout: u64,
    pub bootstrap_token: Option<String>,
    pub renew_before_seconds: u64,
    pub google: Option<SocialProviderConfig>,
    pub facebook: Option<SocialProviderConfig>,
    pub github: Option<SocialProviderConfig>,
}

pub struct SocialProviderConfig {
    pub path: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: Option<String>,
    pub scopes: Vec<String>,
}

pub struct StatelessAuthRuntime {
    pub config: StatelessAuthConfig,
    pub session: SpaSessionRuntime,
}
```

Use Java-compatible serde aliases for camel-case config fields. The primary
file should be `statelessAuth.yml`; accept `statelessAuth.yaml` as a
compatibility fallback.

The serde layer can keep the Java-compatible flat fields, such as
`googlePath`, `googleClientId`, and `googleClientSecret`, and normalize them
into `SocialProviderConfig` entries after load. This keeps config-server
compatibility while giving Rust a cleaner internal model.

### Handler Registration

`apps/light-gateway` already reserves the `stateless` handler id. The runtime
loader should follow the same pattern as MCP:

```rust
let stateless_auth = load_stateless_auth_runtime(
    config,
    active_handlers.is_handler_active("stateless"),
)?;
```

If `stateless` is not active in any chain, the config does not need to be
loaded. If the config is active but `enabled: false`, register the disabled
module and return `None`.

No `@alias` syntax is needed. The handler id in `handler.yml` is the stable
Rust contract.

Example BFF chain:

```yaml
handlers:
  - exception
  - cors
  - stateless
  - header
  - prefix
  - token
  - router

chains:
  default:
    - exception
    - cors
    - stateless
    - header
    - prefix
    - token
    - router
  websocket:
    - exception
    - stateless
    - security
    - websocket

paths:
  - path: /authorization
    method: GET
    exec:
      - default
  - path: /logout
    method: GET
    exec:
      - default
```

The handler should normally run after CORS and before proxy/router/WebSocket.

### Login Flow

For `authPath`:

```text
GET /authorization?code=...&state=...
  -> validate code
  -> generate csrf
  -> call token endpoint with authorization_code grant
  -> verify/parse returned internal access token
  -> set BFF cookies
  -> return { "scopes": [...], "redirectUri": "...?state=...", "denyUri": "..." }
```

Token request mapping should reuse `client.yml`:

- `oauth.token.server_url` or `oauth.token.serviceId`
- `oauth.token.enableHttp2`
- `oauth.token.authorization_code.uri`
- `oauth.token.authorization_code.client_id`
- `oauth.token.authorization_code.client_secret`
- `oauth.token.authorization_code.redirect_uri`
- `oauth.token.authorization_code.scope`

The form body should match Java:

```text
grant_type=authorization_code
code=<code>
redirect_uri=<optional redirect_uri>
csrf=<generated csrf>
scope=<space separated scopes, if configured>
```

### Session Validation Flow

For requests that are not login/logout:

```text
request
  -> read accessToken cookie
  -> verify/parse internal JWT with security.yml rules
  -> extract csrf claim
  -> find request CSRF from X-CSRF-TOKEN, WebSocket subprotocol, or query
  -> compare csrf values
  -> refresh token if exp is inside renew window
  -> inject Authorization: Bearer <access-token>
  -> continue handler chain
```

CSRF source order should match Java:

1. `X-CSRF-TOKEN` header.
2. `Sec-WebSocket-Protocol` value starting with `csrf.` when the request has
   `Sec-WebSocket-Key` and `Sec-WebSocket-Version`.
3. Query parameter `csrf`.

The WebSocket subprotocol behavior is important for browser WebSocket clients
that cannot set arbitrary headers. The auth handler should run before the
`websocket` router so the downstream handshake receives the internal
`Authorization` header.

### Session-Expired Response

The Java handler usually allows requests with no cookies to continue so the
downstream service can decide whether the endpoint is anonymous. Rust should
preserve that pass-through behavior for requests with no session evidence.

When the request does have session evidence but the session cannot be renewed,
for example an expired or rejected refresh token, Rust should clear BFF cookies
and return a structured response using `cookieTimeoutUri`:

```json
{
  "code": "ERR10040",
  "message": "SPA session expired",
  "timeoutUri": "/",
  "authenticated": false
}
```

The status should be `401` unless a later product config explicitly asks for a
different behavior. This gives the SPA a deterministic signal to navigate to
the configured timeout or login page without scraping an Undertow-style status
string.

### Internal JWT Verification

The shared SPA runtime should not call the existing `verify_jwt_request`
function directly. That function is designed for API requests with an
`Authorization` header, path skips, pass-through claims, and normal security
handler behavior.

The SPA auth runtime needs a lower-level token verifier that can:

- verify the access-token signature using the same certificates and algorithms
  as `security.yml`;
- parse claims from a token stored in a cookie;
- optionally ignore expiration while deciding whether the token can be
  refreshed;
- fail hard on invalid signature, invalid algorithm, malformed JWT, and missing
  key;
- return an `AuthPrincipal` and raw claims for CSRF, cookie metadata, and
  optional request-context propagation.

This can be implemented by extracting a reusable helper from `security.rs`,
for example:

```rust
verify_jwt_token(
    runtime: &SecurityRuntime,
    token: &str,
    expiry_mode: JwtExpiryMode,
) -> Result<AuthPrincipal, HandlerRejection>
```

The normal `security` handler can keep its current request-level wrapper, while
SPA auth uses the token-level helper for cookie tokens.

### Social Provider Login

Google, Facebook, and GitHub login are implemented as thin handler entrypoints
that reuse the same cookie/session runtime as the authorization-code callback.
The existing handler ids are kept:

```yaml
chains:
  google:
    - exception
    - correlation
    - cors
    - google
    - stateless
    - header
    - prefix
    - router
  facebook:
    - exception
    - correlation
    - cors
    - facebook
    - stateless
    - header
    - prefix
    - router
  github:
    - exception
    - correlation
    - cors
    - github
    - stateless
    - header
    - prefix
    - router
```

The implemented provider flow is:

1. Match its configured provider path, for example `googlePath`,
   `facebookPath`, or `githubPath`.
2. For Google, exchange the authorization `code` with the Google token endpoint
   and use the returned `id_token` as the subject token. If the provider does
   not return an ID token, fall back to `access_token`.
3. For Facebook, accept the Java-compatible `accessToken` query parameter, or
   exchange an authorization `code` with the Facebook token endpoint.
4. For GitHub, exchange the authorization `code` with the GitHub token
   endpoint.
5. Use `client.yml` `oauth.token.token_exchange` to exchange the provider
   subject token for an internal light-oauth token set with a CSRF claim.
6. Set the same BFF cookies as the generic stateless handler and return the
   same JSON shape.

Provider token endpoints default to the public provider URLs, but can be
overridden for tests or regional deployments:

```yaml
googleTokenEndpoint: ${statelessAuth.googleTokenEndpoint:https://oauth2.googleapis.com/token}
facebookTokenEndpoint: ${statelessAuth.facebookTokenEndpoint:https://graph.facebook.com/v19.0/oauth/access_token}
githubTokenEndpoint: ${statelessAuth.githubTokenEndpoint:https://github.com/login/oauth/access_token}
```

External identity mapping is intentionally delegated to the internal
token-exchange implementation. Once portal-service tokenization has a final
RPC contract, the subject-token exchange can map provider identities there
without changing the gateway cookie/session runtime.

### Refresh Flow

The Java handler refreshes 90 seconds before expiry and deduplicates concurrent
refreshes with `RefreshTokenSingleFlight`. Rust should keep that behavior.

Default Rust settings:

```yaml
renewBeforeSeconds: ${statelessAuth.renewBeforeSeconds:90}
refreshSingleFlightWaitMs: ${statelessAuth.refreshSingleFlightWaitMs:5000}
refreshSingleFlightCacheMs: ${statelessAuth.refreshSingleFlightCacheMs:3000}
refreshSingleFlightMaxEntries: ${statelessAuth.refreshSingleFlightMaxEntries:10000}
```

These fields are Rust improvements. They can be omitted from config-server
templates until a product needs to tune them.

Refresh-token request mapping should reuse `client.yml`
`oauth.token.refresh_token` and send:

```text
grant_type=refresh_token
refresh_token=<cookie refresh token>
csrf=<new csrf>
scope=<space separated scopes, if configured>
```

### Cookies

Cookie names should remain Java-compatible:

| Cookie | HttpOnly | Source |
| --- | --- | --- |
| `accessToken` | true | OAuth access token |
| `refreshToken` | true | OAuth refresh token |
| `csrf` | false | Generated CSRF value |
| `userId` | false | JWT `uid` claim |
| `userType` | false | JWT `userType` claim |
| `roles` | false | Base64-encoded JWT `role` claim, default `user` |
| `host` | false | JWT `host` claim |
| `email` | false | JWT `eml` claim |
| `eid` | false | JWT `eid` claim |

Access-token, user-info, and CSRF cookies should use the access token
`expires_in` value as `Max-Age`. Refresh-token cookie `Max-Age` should use
`sessionTimeout` unless the token response includes a remember value other than
`N`, in which case it should use `rememberMeTimeout`.

Java only clears cookies that were present on the request. Rust should improve
logout by always emitting deletion cookies for the known cookie names, using
the configured domain/path/secure attributes. This avoids stale browser cookies
when a cookie is omitted from a particular request.

Default SameSite should remain `None` for Java parity. Add a Rust-only optional
`cookieSameSite` field with default `None` so deployments can choose `Lax` or
`Strict` when the SPA and BFF are same-site.

## Config Server Model

The config-server should continue to resolve placeholders before startup:

```yaml
statelessAuth.redirectUri: https://localhost:3000/#/app/dashboard
statelessAuth.cookieDomain: localhost
statelessAuth.cookieSecure: true
client.tokenAcClientId: ...
client.tokenAcClientSecret: ...
client.tokenRtClientId: ...
client.tokenRtClientSecret: ...
```

The Rust gateway should only consume the resolved `statelessAuth.yml`,
`client.yml`, `security.yml`, and `handler.yml` files. It should not need to
know whether the values came from product defaults, environment variables, or
light-portal overrides.

## Implemented Surface

- Shared SPA cookie/session runtime, including cookie parser/writer, CSRF
  extraction, JWT claim extraction, and Java-compatible cookie names.
- OAuth token client support for authorization-code, refresh-token, and
  token-exchange grant requests using `client.yml`.
- Refresh-token renewal with a bounded completed-result cache.
- `statelessAuth.yml` loader, module registry registration, active-handler
  gating, and runtime reload.
- `stateless`, `google`, `facebook`, and `github` request handling in
  `light-gateway`.
- Structured session-expired response using `cookieTimeoutUri`.
- Unit/runtime-load coverage for config parsing, cookie attributes, provider
  subject-token selection, active-handler loading, and gateway wiring.
