# MSAL Auth Handler

## Status

The Rust implementation in `light-pingora` and `light-gateway` requires an explicit
MSAL trust policy and a dedicated HTTPS `jwksUri` for RS256 signature verification.
It validates the tenant, issuer, API audience, client, delegated scopes and lifetime
on both login and cookie requests. Signing trust is independent of generic JWT
providers, their caches, and `client.yml` routing or TLS settings.

## Purpose

The `msal-auth` module is an alternative to `msal-exchange` for Microsoft Entra ID single-page application (SPA) architectures where the frontend acts as the primary OAuth client.

In this flow:
1. The SPA handles Microsoft authentication, token acquisition, and token refresh directly.
2. The SPA submits the Entra ID access token to the gateway's `/auth/ms/login` endpoint.
3. The gateway validates the Entra ID access token against the configured MSAL JWKS endpoint and policy and sets the `accessToken` and `csrf` cookies using a signed CSRF value bound to that exact access token.
4. On subsequent API calls, the gateway validates the Microsoft JWT with expiry enforcement, verifies cookie/request agreement and the access-token-bound CSRF HMAC, sets the gateway auth principal for later handlers, and forwards the token in the `Authorization: Bearer` header.

This eliminates the need for an internal `light-oauth` token exchange, reducing infrastructure dependencies while maintaining backend API security.

## Configuration

### `handler.yml`

Register `msal-auth` in the handler chain before handlers that need `ctx.auth` or the downstream `Authorization` header, such as `access-control`, `router`, or proxy handling.

```yaml
handlers:
  - cors
  - msal-auth
  - router

chains:
  bff:
    - cors
    - msal-auth
    - router

paths:
  - path: /auth/ms/login
    method: POST
    exec:
      - bff
  - path: /auth/ms/login
    method: OPTIONS
    exec:
      - bff
  - path: /auth/ms/logout
    method: POST
    exec:
      - bff
  - path: /auth/ms/logout
    method: OPTIONS
    exec:
      - bff

defaultHandlers:
  - cors
  - msal-auth
  - router
```

### `msal-auth.yml`

```yaml
enabled: ${msal-auth.enabled:true}
tenantId: ${msal-auth.tenantId:}
issuer: ${msal-auth.issuer:}
audience: ${msal-auth.audience:}
jwksUri: ${msal-auth.jwksUri:}
requiredScopes: ${msal-auth.requiredScopes:}
allowedClientIds: ${msal-auth.allowedClientIds:}
loginPath: ${msal-auth.loginPath:/auth/ms/login}
logoutPath: ${msal-auth.logoutPath:/auth/ms/logout}
logoutCsrfEnforced: ${msal-auth.logoutCsrfEnforced:true}
cookieDomain: ${msal-auth.cookieDomain:localhost}
cookiePath: ${msal-auth.cookiePath:/}
cookieSecure: ${msal-auth.cookieSecure:false}
sessionTimeout: ${msal-auth.sessionTimeout:3600}
cookieSameSite: ${msal-auth.cookieSameSite:None}
csrfActiveKeyId: ${msal-auth.csrfActiveKeyId:}
csrfKeys: ${msal-auth.csrfKeys:}
websocketOrigins: ${msal-auth.websocketOrigins:}
```

### Migration and trust policy (#438)

An active, enabled handler now rejects incomplete policy configuration during
runtime loading, before the gateway installs it. Invalid reloads return an error
and retain the previous runtime; requests do not rebuild invalid configurations.
An inactive or disabled handler does not require a policy.

1. Expose delegated scopes on your Entra **API** app registration and grant the
   SPA permission to request them.
2. Configure the exact tenant-specific issuer from trusted OpenID metadata,
   the API audience, the required scope names, the allowed SPA client IDs, and
   `jwksUri` from the same trusted metadata document. The JWKS endpoint must use
   HTTPS without credentials or a fragment; its TLS certificate is verified.
3. Have MSAL request the API's scopes (for example
   `api://<api-id>/access_as_user`) and send the resulting **access token** to
   `/auth/ms/login`. Do not send an ID token or a Microsoft Graph access token.
4. Deploy the values with the code change. Existing cookies must satisfy the
   policy on their next request; rejected authentication clears both session
   cookies so the SPA can acquire a new token and log in again.

Example `values.yml` (replace the example UUIDs):

```yaml
msal-auth.tenantId: "11111111-1111-1111-1111-111111111111"
msal-auth.issuer: "https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/v2.0"
msal-auth.audience: "22222222-2222-2222-2222-222222222222"
msal-auth.jwksUri: "https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/discovery/v2.0/keys"
msal-auth.requiredScopes: ["access_as_user"]
msal-auth.allowedClientIds: ["33333333-3333-3333-3333-333333333333"]
```

Both list settings accept native arrays, JSON/YAML array strings, or comma-separated
strings through the existing values/environment substitution mechanism. Empty
defaults deserialize successfully but fail enabled-policy validation. Every
required scope must occur as a complete, case-sensitive entry in `scp`.

One tenant/issuer/version is supported per MSAL runtime:

- v2: exact HTTPS issuer ending in `/<tenantId>/v2.0`, `ver: "2.0"`, and the
  permitted client in `azp`; use the API's application ID as audience.
- v1: exact HTTPS metadata issuer ending in `/<tenantId>/`, `ver: "1.0"`, and
  the permitted client in `appid`; configure the API's expected audience.
- Tenant/client IDs must be canonical non-nil UUIDs. The issuer's tenant path
  must match the configured tenant. Hosts are not hard-coded, to permit trusted
  sovereign-cloud metadata; an issuer typo fails exact token matching.
- A nonempty subject and delegated scopes are required. ID tokens and app-only
  tokens cannot substitute for delegated access tokens. If `idtyp` is present,
  it must be `user`; a `typ: JWT` header alone does not identify token purpose.
- Expiration and optional not-before are enforced with
  `security-msal.jwt.clockSkewInSeconds`, even when the generic verifier has
  `ignoreJwtExpiry: true`. Generic flags cannot disable MSAL signature or lifetime checks.

Like the final Java implementation in `light-spa-4j` PR #182, direct MSAL
authentication permits **RS256 only** and has no token/principal cache. Each
runtime owns its JWKS cache and an independent HTTPS client. Shared providers,
including providers with the same `kid`, cannot supply MSAL signing keys.
`client.yml` key routing, provider selection and TLS overrides are not consulted.

JWKS requests have a five-second connect/total timeout and a 1 MiB response
limit. Redirects are rejected. Keys expire after one hour; unknown keys and
signature failures can refresh keys, with a 30-second minimum interval between
fetch attempts, including failed attempts. Concurrent lookups share the refresh
lock. Reloading creates a fresh verifier bound to the new endpoint. Resolver
failures remain generic 5xx responses and retain existing session cookies.

### `security-msal.yml`

`security-msal.yml` supplies `jwt.clockSkewInSeconds` (60 seconds by default).
Its generic issuer/audience, cache, mock, relaxed-key and verification-disable
settings do not override the dedicated MSAL trust policy. Missing settings use
defaults. Other handlers such as `msal-exchange` continue using their existing
security runtime.

```yaml
enableVerifyJwt: ${security-msal.enableVerifyJwt:true}
ignoreJwtExpiry: ${security-msal.ignoreJwtExpiry:false}
enableRelaxedKeyValidation: ${security-msal.enableRelaxedKeyValidation:false}
issuer: ${security-msal.issuer:}
audience: ${security-msal.audience:}
jwt:
  clockSkewInSeconds: ${security-msal.jwt.clockSkewInSeconds:60}
```

## Handlers

- **Login (`/auth/ms/login`)**: Expects an Entra ID token in the `Authorization: Bearer` header. Validates its RS256 signature using `msal-auth.jwksUri`, then enforces the MSAL policy and lifetime. Generates a secure CSRF token and returns both `accessToken` and `csrf` as `Set-Cookie` headers.
- **Logout (`POST /auth/ms/logout`)**: Validates logout CSRF when configured,
  clears every cookie the runtime sets (`accessToken` and `csrf`), and returns
  `204 No Content` without a response body or content type.
- **Session Validation (any path with cookies)**: Reads the `accessToken` cookie. Validates the JWT with expiry enforcement. Checks cookie/request agreement and the signed CSRF binding to those exact access-token bytes. If valid, it sets the gateway auth principal and forwards the `accessToken` downstream in the `Authorization: Bearer` header.

## Frontend Integration

The Single Page Application (SPA) must coordinate with the gateway for session creation and destruction.

### Login Request

When the SPA acquires an access token from Microsoft Entra ID (e.g., using
MSAL.js), it must send that token to the gateway's login endpoint to establish
the secure HTTP-only cookies. Both login and logout use `POST`; neither needs a
request body. A zero-length body is also accepted when a shared client sets
`Content-Type: application/json`.

For cross-origin deployments, both examples require preflight: login sends the
non-safelisted `Authorization` header and CSRF-protected logout sends the
non-safelisted `X-CSRF-TOKEN` header. Keep the explicit `OPTIONS` routes and
qualify the exact origin, credentials, method, and requested headers.

```javascript
async function gatewayLogin(entraIdToken) {
  const response = await fetch('/auth/ms/login', {
    method: 'POST',
    credentials: 'include',
    headers: {
      'Authorization': `Bearer ${entraIdToken}`
    }
  });

  if (!response.ok) {
    throw new Error('Failed to create gateway session');
  }
  
  console.log('Gateway session established');
}
```

### Logout Request

When the user logs out, the SPA must call the gateway's logout endpoint to
clear the HTTP-only session cookies. Send credentials and the
`X-CSRF-TOKEN` header read from the readable `csrf` cookie; no body is needed.

```javascript
// Helper to read the csrf cookie
function getCookie(name) {
  const value = `; ${document.cookie}`;
  const parts = value.split(`; ${name}=`);
  if (parts.length === 2) return parts.pop().split(';').shift();
}

async function gatewayLogout() {
  const csrfToken = getCookie('csrf');
  
  const response = await fetch('/auth/ms/logout', {
    method: 'POST',
    credentials: 'include',
    headers: {
      'X-CSRF-TOKEN': csrfToken
    }
  });

  if (response.status !== 204) {
    throw new Error(`Unexpected logout status ${response.status}`);
  }

  console.log('Gateway session cleared');
}
```

Login and logout are POST-only. A legacy GET or any other unsupported method
returns `405`, `ERR10008`, and `Allow: POST` before authentication or cookie
side effects. Keep explicit `OPTIONS` routes permanently with `cors` before
`msal-auth` in the selected chain.

## Error Handling

| Code | Meaning |
| --- | --- |
| `ERR10008` | Method is not allowed; MSAL login/logout responses advertise `Allow: POST`. |
| `ERR10036` | Logout CSRF header is missing when enforcement is enabled. |
| `ERR11649` | Logout CSRF cookie/header validation failed without exposing either value. |
| `ERR10000` | Invalid Microsoft access token. Login issues no cookies; cookie authentication clears the rejected session. |
| `ERR10056` | Key resolution is unavailable. Preserves the verifier's 5xx status and does not clear session cookies. |

Authentication responses deliberately use one generic message. Logs contain a
bounded failure category (issuer, tenant, audience, version, client, scopes,
purpose, lifetime, or signature/standard claims), never token or claim values.
CSRF failures do not clear valid session cookies. Downstream handlers remain
responsible for operation-specific authorization.

### Regression validation

```sh
cargo test -p light-pingora --lib msal_auth
cargo test -p light-security --lib
```

Tests use locally signed JWTs and Pingora request sessions, not live Entra tokens.
They cover login and cookie paths, cache-disabled and cache-enabled verification,
warm-cache rejection, expiry overrides, clock-skew boundaries, cookie recovery,
and loading the shipped template with substituted values. These checks do not
qualify a deployed Entra integration.

### API Request

For standard API calls to backend services, the browser will automatically include the HTTP-only `accessToken` cookie. However, any request that modifies state or requires CSRF protection must include the CSRF token. The SPA must read the `csrf` cookie and append it as the `X-CSRF-TOKEN` header.

```javascript
async function callBackendApi(endpoint, data) {
  const csrfToken = getCookie('csrf');
  
  const response = await fetch(endpoint, {
    method: 'POST', // or PUT, DELETE, etc.
    headers: {
      'Content-Type': 'application/json',
      'X-CSRF-TOKEN': csrfToken
    },
    body: JSON.stringify(data)
  });

  if (!response.ok) {
    throw new Error('API call failed');
  }

  return response.json();
}
```

### WebSocket Connection

The browser's native `WebSocket` API does not allow setting custom HTTP headers. To pass the CSRF token during the WebSocket handshake upgrade, the SPA must pass it as a subprotocol string prefixed with `csrf.`. The gateway will extract and validate it.

```javascript
function connectWebSocket(path) {
  const csrfToken = getCookie('csrf');
  
  // Create a subprotocol string that the gateway recognizes
  const csrfProtocol = `csrf.${csrfToken}`;
  
  // Note: Depending on your WebSocket server, you may also need to pass 
  // the actual subprotocol you intend to use (e.g., 'wamp', 'graphql-ws')
  // alongside the csrf protocol.
  const ws = new WebSocket(`wss://api.example.com${path}`, ['light-spa-v1', csrfProtocol]);

  ws.onopen = () => {
    console.log('WebSocket connected securely');
  };

  ws.onerror = (error) => {
    console.error('WebSocket connection failed (possible CSRF or Auth issue)', error);
  };

  return ws;
}
```

## Session-bound CSRF (#440)

`msal-auth` signs `v1.<key-id>.<nonce>.<tag>` using HMAC-SHA-256. The nonce
contains 32 OS-random bytes. The MAC covers a domain label, length-delimited
version/key/nonce metadata, and the SHA-256 digest of the exact Microsoft access
token bytes. Both cookie/request agreement and a valid MAC are required. Microsoft
signature, trust policy and lifetime verification still precede API authorization.
Two logins with identical access-token bytes share the same binding boundary.
Unsigned legacy values, unknown key IDs, tampering and cross-token substitution
are rejected. A replacement access token requires a newly issued CSRF value.

HTTP requests, including logout, require `X-CSRF-TOKEN`. Any `csrf` or
`X-CSRF-TOKEN` query parameter is rejected, including an empty value or a request
with a valid header. Clients must stop placing credentials in URLs; rejection
cannot erase URLs already recorded by an upstream proxy.

For actual HTTP/1.1 WebSocket upgrades, clients offer
`['light-spa-v1', 'csrf.' + csrf]`. The fixed `light-spa-v1` protocol is terminated
at the gateway and can be selected without echoing the credential. The CSRF
protocol is stripped before upstream forwarding and is never selected in the
response. Configured application protocols may still be negotiated normally.
Duplicate CSRF offers/headers/cookies, conflicting header/protocol values, invalid
upgrades and absent, `null`, malformed or untrusted Origins fail closed.

Set `websocketOrigins` to exact canonical origins, such as
`["https://localhost:3000"]`, for each SPA auth handler and retain the
`websocket-router.yml` path-specific `originAllowlist`. Both policies must allow
the Portal. Empty auth origin lists deny cookie-authenticated WebSockets. Neither
policy is derived from Host or forwarding headers.

### Key provisioning and rotation

Enabled `msal-auth` requires `csrfActiveKeyId` and `csrfKeys`. The latter is a
map of key IDs to unpadded Base64URL-encoded keys (32–128 decoded bytes per key).
IDs contain 1–32 ASCII letters, digits, underscores or hyphens. A native map or
JSON/YAML map string can be supplied through the existing values and secret
substitution/decryption pipeline. Typed Debug and module diagnostics redact keys.
There is no development default or per-process generated signing key.

Generate each key with an OS-backed tool in your secret-provisioning environment:

```sh
openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n'
```

Store the result in your deployment secret source, with the logical shape below;
the placeholder is deliberately not a valid deployable key:

```yaml
msal-auth.csrfActiveKeyId: key-2026-10
msal-auth.csrfKeys:
  key-2026-10: <secret-source-value>
msal-auth.websocketOrigins: ["https://localhost:3000"]
```

Provision the same key ring on all replicas. To rotate, first distribute the new
key to all replicas while the old key remains active; then switch the active ID.
Accept the previous key only for the explicitly planned session/rollback window,
then remove it everywhere. Removal invalidates sessions signed by that key.
Startup rejects missing/invalid keys. Reload constructs and validates a complete
replacement runtime before swapping its Arc snapshot; an invalid candidate returns
an error and leaves the active keys and runtime installed. Concurrent requests use
one complete runtime snapshot. Explicitly disabling a handler remains an
administrator action, not an invalid-config fallback.

### Enforcement and rollout

Logout enforcement now defaults to `true` across `msal-auth`, `msal-exchange` and
`stateless-auth`, including gateway templates. Explicit existing `false` values
remain observation-only and **do not provide logout CSRF protection**. Provision
keys, exact Origins, header-based logout clients and the Portal `light-spa-v1`
WebSocket offer before switching traffic. Existing unsigned direct-MSAL sessions
must log in again. Coordinate all replicas and remove explicit observation-only
values at cutover. Secure cookie flags and host/path/domain scope remain deployment
requirements; cookie names (`csrf`, `accessToken`) have not changed.

Enforced direct-MSAL logout requires cookie/request agreement plus a valid MAC
bound to the accompanying access-token bytes. This allows cleanup of expired
sessions without granting API access. No-session logout is idempotent; malformed
and partial cookie sets fail closed. JWT-backed logout verifies the application
JWT signature and CSRF claim, ignoring expiry only in the cleanup path.
Refresh-token-only sessions cannot prove CSRF binding and must log in again for
API access or enforced logout. Administrators can clear expired browser cookies
through the browser's site-data controls when no authenticated logout context
remains.

## Refresh Flow

Unlike `msal-exchange` or `stateless-auth`, `msal-auth` does not issue or manage refresh tokens. The SPA is responsible for using MSAL.js to silently refresh the Entra ID token and calling `/auth/ms/login` again to update the session cookies before they expire.

## Reload Behavior

The gateway reloads `msal-auth` when `handler.yml`, `msal-auth.yml`, or `security-msal.yml` changes. Reloading `security-msal.yml` refreshes both `msal-auth` and `msal-exchange` because both handlers validate Microsoft tokens with that security runtime.
