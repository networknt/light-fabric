# MSAL Exchange Handler

## Status

Initial Rust implementation is complete in `light-pingora` and
`light-gateway`. It includes config loading, named `security-msal.yml`
validation support, token-exchange handling, shared SPA session/cookie/CSRF
logic, logout, refresh-token renewal, handler wiring, config stubs, and
runtime-load tests.

## Purpose

The Java `light-spa-4j` `msal-exchange` module is the on-prem BFF login bridge
for SPA deployments that use Microsoft Authentication Library SSO. The browser
uses MSAL.js to obtain a Microsoft token, sends that token to the gateway, and
the gateway exchanges it for an internal light-oauth token set. After exchange,
the browser session behaves the same as the stateless authorization-code
handler: internal tokens are stored in cookies, CSRF is validated on subsequent
requests, refresh tokens keep the session alive, and the gateway injects
`Authorization: Bearer <internal-token>` before routing downstream.

In light-fabric this should be a `light-pingora` security handler in
`light-gateway`. It should share most of its implementation with
`stateless-auth.md`; only the initial login exchange differs.

## Goals

- Preserve the Java MSAL token-exchange flow.
- Keep `msal-exchange.yml` field names recognizable for light-portal and
  config-server product configuration.
- Validate the incoming Microsoft token with a separate `security-msal.yml`
  runtime before token exchange.
- Exchange the Microsoft token with light-oauth using `client.yml`
  `oauth.token.token_exchange`.
- Store the returned internal token set in the same Java-compatible cookies as
  the stateless handler.
- Share CSRF validation, cookie writing, logout, refresh-token renewal, and
  downstream `Authorization` injection with the stateless handler.
- Add a stable `msal-exchange` handler id to `light-gateway`.
- Register loaded config in `ModuleRegistry` and fail startup on invalid active
  configuration.

## Non-Goals

- Do not forward the Microsoft token to downstream services after exchange.
- Do not implement a server-side browser session store.
- Do not merge MSAL token validation into the normal downstream `security`
  handler. MSAL validation applies only to the exchange endpoint.
- Do not invent a REST-specific tokenization or portal-service client in this
  handler. The only outbound call is the OAuth token-exchange request.
- Do not require a separate BFF binary.

## Resolved Decisions

- Support `subjectTokenType` in both `client.yml` and `msal-exchange.yml`.
  The handler-specific value takes precedence when set, and `client.yml`
  remains the shared OAuth token-exchange default.
- Support strict Microsoft token validation in `security-msal.yml` when a
  deployment needs issuer and audience checks.

## Java Behavior To Map

Java config file:

```yaml
enabled: ${msal-exchange.enabled:true}
exchangePath: ${msal-exchange.exchangePath:/auth/ms/exchange}
logoutPath: ${msal-exchange.logoutPath:/auth/ms/logout}
cookieDomain: ${msal-exchange.cookieDomain:localhost}
cookiePath: ${msal-exchange.cookiePath:/}
cookieSecure: ${msal-exchange.cookieSecure:false}
sessionTimeout: ${msal-exchange.sessionTimeout:3600}
rememberMeTimeout: ${msal-exchange.rememberMeTimeout:604800}
```

Java also loads a separate security config named `security-msal`:

```text
SecurityConfig.load("security-msal")
```

This config verifies the incoming Microsoft token. The normal `security.yml`
runtime verifies/parses internal light-oauth access tokens used in cookies.

Java request behavior:

- `exchangePath`, normally `/auth/ms/exchange`, requires
  `Authorization: Bearer <microsoft-token>`.
- Missing bearer token returns `ERR11000`.
- The handler verifies the Microsoft token with `security-msal.yml`.
- Verification failure returns `ERR10000`.
- The handler generates a CSRF value and sends an OAuth token-exchange request
  with the Microsoft token as `subject_token`.
- Token-exchange failure returns `ERR11001`.
- On success, the handler sets the same BFF cookies as the stateless handler
  and returns JSON containing `scopes`.
- `logoutPath`, normally `/auth/ms/logout`, clears BFF cookies and ends the
  request.
- Subsequent requests use the same cookie, CSRF, refresh, and downstream
  `Authorization` injection flow as the stateless handler.

Java error codes to preserve:

| Code | Meaning |
| --- | --- |
| `ERR11000` | Microsoft bearer token is missing |
| `ERR11001` | Internal token exchange failed |
| `ERR10000` | Incoming Microsoft token or returned internal token is invalid |
| `ERR10036` | CSRF token is missing from request |
| `ERR10038` | CSRF claim is missing from JWT |
| `ERR10039` | Request CSRF and JWT CSRF do not match |

## Rust Architecture

Use the shared SPA auth runtime described in `stateless-auth.md`.

Proposed modules:

```text
frameworks/light-pingora/src/spa_auth.rs
frameworks/light-pingora/src/msal_exchange.rs
```

`msal_exchange.rs` owns only the Microsoft-token exchange entrypoint:

```rust
pub struct MsalExchangeConfig {
    pub enabled: bool,
    pub exchange_path: String,
    pub logout_path: String,
    pub cookie_domain: String,
    pub cookie_path: String,
    pub cookie_secure: bool,
    pub session_timeout: u64,
    pub remember_me_timeout: u64,
    pub renew_before_seconds: u64,
    pub subject_token_type: String,
}

pub struct MsalExchangeRuntime {
    pub config: MsalExchangeConfig,
    pub session: SpaSessionRuntime,
    pub msal_security: SecurityRuntime,
}
```

Use `msal-exchange.yml` as the primary file name and accept
`msal-exchange.yaml` as a compatibility fallback.

The `SecurityRuntime` loader should be generalized so the MSAL handler can load
a named security config:

```rust
load_security_runtime_from_file(
    runtime_config,
    "security-msal.yml",
    "light-pingora/security-msal",
    "security-msal",
    active,
)
```

That keeps normal downstream JWT behavior on `security.yml` while the exchange
endpoint validates Microsoft tokens against `security-msal.yml`.

### Handler Registration

Add `msal-exchange` to `apps/light-gateway` handler descriptors as a security
handler:

```rust
("msal-exchange", PingoraHandlerKind::Security)
```

The primary handler id should be `msal-exchange`. No `@alias` syntax is
needed. An additional short alias such as `msal` can be added later only if a
real product config needs it.

Runtime loading should follow the existing active-handler model:

```rust
let msal_exchange = load_msal_exchange_runtime(
    config,
    active_handlers.is_handler_active("msal-exchange"),
)?;
```

If the handler is not active in `handler.yml`, no MSAL config is required. If
the handler is active and its config is invalid, startup should fail. If
`enabled: false`, register the disabled module and return `None`.

Example chain:

```yaml
handlers:
  - exception
  - cors
  - msal-exchange
  - header
  - prefix
  - token
  - router

chains:
  bff:
    - exception
    - cors
    - msal-exchange
    - header
    - prefix
    - token
    - router
  websocket:
    - exception
    - msal-exchange
    - security
    - websocket

paths:
  - path: /auth/ms/exchange
    method: POST
    exec:
      - bff
  - path: /auth/ms/logout
    method: GET
    exec:
      - bff
```

### Exchange Flow

For `exchangePath`:

```text
POST /auth/ms/exchange
Authorization: Bearer <microsoft-token>

  -> extract bearer token
  -> verify Microsoft token with security-msal.yml
  -> generate csrf
  -> call light-oauth token endpoint with token-exchange grant
  -> verify/parse returned internal access token
  -> set BFF cookies
  -> return { "scopes": [...] }
```

The token-exchange request should use `client.yml`
`oauth.token.token_exchange`:

- `oauth.token.server_url` or `oauth.token.serviceId`
- `oauth.token.enableHttp2`
- `oauth.token.token_exchange.uri`
- `oauth.token.token_exchange.client_id`
- `oauth.token.token_exchange.client_secret`
- `oauth.token.token_exchange.scope`
- `oauth.token.token_exchange.subjectTokenType` as the default subject token
  type when the handler config does not override it

The form body should match Java and the `http-client` composer:

```text
grant_type=urn:ietf:params:oauth:grant-type:token-exchange
subject_token=<microsoft-token>
subject_token_type=urn:ietf:params:oauth:token-type:jwt
csrf=<generated csrf>
requested_token_type=<optional requested token type>
audience=<optional audience>
scope=<space separated scopes, if configured>
```

The handler should set `Authorization: Basic <client_id:client_secret>` on the
outbound token-exchange request.

### Session Validation Flow

After exchange, MSAL and stateless auth must use the same downstream request
flow:

```text
request
  -> read accessToken cookie
  -> verify/parse internal JWT with security.yml
  -> validate CSRF from request against JWT csrf claim
  -> refresh internal token when it is inside the renew window
  -> inject Authorization: Bearer <internal-access-token>
  -> continue handler chain
```

CSRF source order should be identical to the stateless handler:

1. `X-CSRF-TOKEN` header.
2. `Sec-WebSocket-Protocol` value starting with `csrf.` when the request has
   `Sec-WebSocket-Key` and `Sec-WebSocket-Version`.
3. Query parameter `csrf`.

The MSAL handler must never inject the Microsoft token downstream. The only
downstream bearer token after login is the internal light-oauth token.

### Internal JWT Verification

MSAL exchange should use the same lower-level token verifier as stateless auth
for internal cookie tokens. It should not use the request-oriented
`verify_jwt_request` wrapper because the token source is a cookie, not an
`Authorization` header.

The shared verifier should validate signature and key material from
`security.yml`, parse claims for CSRF and user cookies, and support an
expiry-mode option so the refresh path can inspect tokens close to expiry
without treating that as a downstream API authentication success.

### Cookies

MSAL exchange should use the same cookie contract as stateless auth:

| Cookie | HttpOnly | Source |
| --- | --- | --- |
| `accessToken` | true | Internal OAuth access token |
| `refreshToken` | true | Internal OAuth refresh token |
| `csrf` | false | Generated CSRF value |
| `userId` | false | JWT `uid` claim |
| `userType` | false | JWT `userType` claim |
| `roles` | false | Base64-encoded JWT `role` claim, default `user` |
| `host` | false | JWT `host` claim |
| `email` | false | JWT `eml` claim |
| `eid` | false | JWT `eid` claim |

For Java parity, keep `cookieSecure` defaulting to `false` in
`msal-exchange.yml`, but production config should set it to `true` when the BFF
is served over HTTPS.

Rust should share the logout improvement from stateless auth: always emit
deletion cookies for known cookie names rather than only clearing cookies that
were present on the request.

### Security Config

`security-msal.yml` should be treated as an active handler dependency when
`msal-exchange` is active. Missing or invalid config should fail startup
because the gateway would otherwise accept an exchange endpoint without a
working Microsoft-token verifier.

Recommended distinction:

- `security-msal.yml`: verifies the incoming Microsoft token on
  `exchangePath`.
- `security.yml`: verifies/parses internal light-oauth tokens in BFF cookies
  and is also used by normal API security handlers.

The Java code skips audience verification for MSAL in the current call path.
Rust should preserve compatibility unless `security-msal.yml` explicitly
configures audience validation support. That keeps on-prem deployments working
when the Microsoft token audience is the SPA client id rather than the BFF.

When a product requires stricter validation, `security-msal.yml` should be able
to require issuer and audience checks for the incoming Microsoft token. The
initial implementation can add these checks to the named `SecurityRuntime`
loader as optional fields:

```yaml
issuer: ${security-msal.issuer:}
audience: ${security-msal.audience:}
```

Blank values preserve the Java-compatible relaxed behavior. Non-blank values
must be enforced during exchange-path token verification, and invalid
issuer/audience should return the same invalid-token error path as other
Microsoft token verification failures.

## Config Server Model

Light-portal should manage the product config values and config-server should
deliver resolved files:

```yaml
msal-exchange.exchangePath: /auth/ms/exchange
msal-exchange.logoutPath: /auth/ms/logout
msal-exchange.cookieDomain: localhost
msal-exchange.cookieSecure: true
msal-exchange.subjectTokenType: urn:ietf:params:oauth:token-type:jwt
client.tokenExClientId: ...
client.tokenExClientSecret: ...
client.subjectTokenType: urn:ietf:params:oauth:token-type:jwt
security-msal.issuer: https://login.microsoftonline.com/{tenant-id}/v2.0
security-msal.audience: <spa-client-id>
```

The gateway consumes only the resolved files:

- `handler.yml`
- `msal-exchange.yml`
- `security-msal.yml`
- `security.yml`
- `client.yml`

## Implemented Surface

- Shared SPA auth runtime from `stateless-auth.md`.
- Named `SecurityRuntime` loading for `security-msal.yml`.
- Token-exchange support in the shared OAuth token client.
- `msal-exchange.yml` parsing, module registry registration, active-handler
  gating, and runtime reload.
- `msal-exchange` request handling in `light-gateway`.
- Required bearer-token extraction, Microsoft token validation,
  token-exchange request, Java-compatible cookie writing, logout, refresh
  renewal, and downstream internal `Authorization` injection.
- Optional issuer/audience validation through `security-msal.yml`.
- Unit/runtime-load coverage for subject-token-type precedence and gateway
  wiring.
