# MSAL Exchange

The `msal-exchange` handler is a BFF security handler for SPA applications
that authenticate with Microsoft Authentication Library, MSAL, and need an
internal light-oauth security profile for gateway authorization.

The SPA obtains an Azure MSAL token in the browser, sends it to the gateway,
and the gateway exchanges it with light-oauth for an internal access token and,
optionally, a refresh token. The internal token set is stored in secure BFF
cookies and is used on later requests together with CSRF protection.

This page documents the current behavior and the token placement extension for
deployments that must keep the Azure MSAL access token in the downstream
`Authorization` header while forwarding the light-oauth token in a separate
header.

## Use Cases

Use `msal-exchange` when:

- The UI is a browser SPA using MSAL.js.
- Azure Entra ID is the identity provider for the browser login.
- The gateway must exchange the Azure token for a light-oauth token containing
  the enterprise security profile and custom claims.
- The gateway must protect browser requests with HttpOnly cookies and CSRF.
- Downstream routing needs either the light-oauth token or the Azure MSAL token
  in the `Authorization` header.

## Handler Placement

Enable the handler in the gateway handler chain before downstream routing and
before handlers that depend on the authenticated principal.

Example:

```yaml
handlers:
  - exception
  - cors
  - msal-exchange
  - header
  - prefix
  - router

chains:
  bff:
    - exception
    - cors
    - msal-exchange
    - header
    - prefix
    - router

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

When the handler is active, the gateway needs these resolved config files:

- `msal-exchange.yml`
- `security-msal.yml`
- `security.yml`
- `client.yml`

`security-msal.yml` validates Azure MSAL tokens. `security.yml` validates the
light-oauth tokens stored in BFF cookies. `client.yml` provides the
light-oauth token-exchange client configuration.

## Exchange Flow

The exchange endpoint receives the Azure token from the SPA and creates the BFF
session.

```text
POST /auth/ms/exchange
Authorization: Bearer <azure-msal-token>

  -> read the Azure bearer token
  -> verify it with security-msal.yml
  -> generate a CSRF value
  -> call light-oauth with the token-exchange grant
  -> verify the returned light-oauth access token with security.yml
  -> set BFF cookies
  -> return { "scopes": [...] }
```

The token-exchange request uses `client.yml` `oauth.token.token_exchange`.
The outgoing form body contains:

```text
grant_type=urn:ietf:params:oauth:grant-type:token-exchange
subject_token=<azure-msal-token>
subject_token_type=urn:ietf:params:oauth:token-type:jwt
csrf=<generated-csrf>
```

`subjectTokenType` can be set in `msal-exchange.yml`. When it is blank, the
shared token client default from `client.yml` is used.

On success, the response body contains the scopes from the light-oauth token:

```json
{
  "scopes": ["scope1", "scope2"]
}
```

## Session Cookies

The handler uses the same cookie contract as the stateless SPA auth handler.

| Cookie | HttpOnly | Description |
| --- | --- | --- |
| `accessToken` | true | light-oauth access token |
| `refreshToken` | true | light-oauth refresh token, when returned |
| `csrf` | false | Generated CSRF value |
| `userId` | false | User id from `uid`, `user_id`, or `sub` |
| `userType` | false | User type from `userType` |
| `roles` | false | Base64 encoded role value, default `user` |
| `host` | false | Host claim |
| `email` | false | Email claim from `eml` |
| `eid` | false | Enterprise id claim |

`accessToken` and `refreshToken` are HttpOnly so browser JavaScript cannot read
the light-oauth tokens. The SPA reads the non-HttpOnly `csrf` cookie and sends
it back with protected requests.

## CSRF Validation

For normal protected requests, the handler validates the request CSRF value
against the `csrf` claim in the light-oauth access token.

CSRF source order:

1. `X-CSRF-TOKEN` request header.
2. `Sec-WebSocket-Protocol` value starting with `csrf.` for WebSocket requests.
3. `csrf` query parameter.

If the CSRF value is missing or does not match the JWT claim, the request is
rejected.

## Token Placement

`authorizationToken` selects which token owns the downstream `Authorization`
header after the BFF session has been established.

Supported values:

| Value | `Authorization` header | Light-oauth token location | Use case |
| --- | --- | --- | --- |
| `light-oauth` | `Bearer <light-oauth-token>` | `Authorization` | Existing enterprise BFF pattern |
| `azure-msal` | `Bearer <azure-msal-token>` | `lightTokenHeader`, default `X-Light-Token` | Azure-whitelisted downstream systems, such as AWS Agent Core |

### `authorizationToken: light-oauth`

This is the current default behavior.

After the exchange, the SPA calls the gateway with cookies and CSRF:

```text
GET /api/orders
Cookie: accessToken=...; csrf=...
X-CSRF-TOKEN: <csrf>
```

The handler:

```text
  -> reads the light-oauth accessToken cookie
  -> verifies it with security.yml
  -> validates CSRF
  -> refreshes the token if it is close to expiry
  -> injects Authorization: Bearer <light-oauth-token>
  -> continues the handler chain
```

Downstream services receive:

```text
Authorization: Bearer <light-oauth-token>
```

This mode is appropriate when downstream services and MCP tools trust
light-oauth directly and expect fine-grained security claims in the normal
`Authorization` header.

### `authorizationToken: azure-msal`

This is the new token placement pattern.

The exchange endpoint is unchanged. The SPA still sends the Azure token to
`/auth/ms/exchange`, and the gateway still stores the returned light-oauth
token in HttpOnly cookies.

For later protected requests, the SPA sends the current Azure MSAL access token
in `Authorization`, plus cookies and CSRF:

```text
GET /agent/chat
Authorization: Bearer <azure-msal-token>
Cookie: accessToken=...; csrf=...
X-CSRF-TOKEN: <csrf>
```

The handler should:

```text
  -> verify the Azure bearer token with security-msal.yml
  -> read the light-oauth accessToken cookie
  -> verify the light-oauth token with security.yml
  -> validate CSRF
  -> refresh the light-oauth token if it is close to expiry
  -> preserve Authorization: Bearer <azure-msal-token>
  -> inject X-Light-Token: Bearer <light-oauth-token>
  -> continue the handler chain
```

Downstream systems receive both tokens:

```text
Authorization: Bearer <azure-msal-token>
X-Light-Token: Bearer <light-oauth-token>
```

This mode is intended for systems that only allow Azure as the OAuth provider
for the normal `Authorization` header, while still needing the light-oauth
security profile for API and MCP authorization decisions.

The SPA should not read or send `X-Light-Token` itself. The gateway should
derive that header from the HttpOnly light-oauth cookie after CSRF validation.
That keeps the light-oauth token out of browser JavaScript.

If a downstream light-gateway is responsible for fine-grained authorization,
it must be configured to verify `X-Light-Token` as the light-oauth token or to
promote `X-Light-Token` to `Authorization` at a trusted boundary before the
normal security/access-control handlers run.

## Configuration

Example default configuration:

```yaml
enabled: ${msal-exchange.enabled:true}
exchangePath: ${msal-exchange.exchangePath:/auth/ms/exchange}
logoutPath: ${msal-exchange.logoutPath:/auth/ms/logout}
cookieDomain: ${msal-exchange.cookieDomain:localhost}
cookiePath: ${msal-exchange.cookiePath:/}
cookieSecure: ${msal-exchange.cookieSecure:false}
sessionTimeout: ${msal-exchange.sessionTimeout:3600}
rememberMeTimeout: ${msal-exchange.rememberMeTimeout:604800}
renewBeforeSeconds: ${msal-exchange.renewBeforeSeconds:90}
refreshSingleFlightWaitMs: ${msal-exchange.refreshSingleFlightWaitMs:5000}
refreshSingleFlightCacheMs: ${msal-exchange.refreshSingleFlightCacheMs:3000}
refreshSingleFlightMaxEntries: ${msal-exchange.refreshSingleFlightMaxEntries:10000}
cookieSameSite: ${msal-exchange.cookieSameSite:None}
cookieTimeoutUri: ${msal-exchange.cookieTimeoutUri:/}
subjectTokenType: ${msal-exchange.subjectTokenType:}
authorizationToken: ${msal-exchange.authorizationToken:light-oauth}
lightTokenHeader: ${msal-exchange.lightTokenHeader:X-Light-Token}
```

Fields:

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Enables or disables the handler once it is active in the chain. |
| `exchangePath` | `/auth/ms/exchange` | Endpoint that receives the Azure MSAL bearer token and creates the BFF session. |
| `logoutPath` | `/auth/ms/logout` | Endpoint that clears BFF cookies. |
| `cookieDomain` | `localhost` | Cookie domain for session cookies. |
| `cookiePath` | `/` | Cookie path for session cookies. |
| `cookieSecure` | `false` | Adds the `Secure` cookie attribute. Use `true` for HTTPS deployments. |
| `sessionTimeout` | `3600` | Default max age in seconds for session cookies. |
| `rememberMeTimeout` | `604800` | Max age in seconds for long-lived refresh-token cookies when light-oauth returns remember-me behavior. |
| `renewBeforeSeconds` | `90` | Refresh the light-oauth access token when it expires within this window. |
| `refreshSingleFlightWaitMs` | `5000` | Maximum wait time for concurrent refresh requests sharing the same refresh token. |
| `refreshSingleFlightCacheMs` | `3000` | Short cache window for a successful refresh result. |
| `refreshSingleFlightMaxEntries` | `10000` | Maximum refresh single-flight cache entries. |
| `cookieSameSite` | `None` | Cookie SameSite attribute. Supported values are `None`, `Lax`, and `Strict`. |
| `cookieTimeoutUri` | `/` | URI returned when the session expires and cannot be refreshed. |
| `subjectTokenType` | blank | Optional token-exchange subject token type override. |
| `authorizationToken` | `light-oauth` | Token to place in downstream `Authorization`: `light-oauth` or `azure-msal`. |
| `lightTokenHeader` | `X-Light-Token` | Header used for the light-oauth token when `authorizationToken` is `azure-msal`. |

Invalid `authorizationToken` values should fail startup. `lightTokenHeader`
should not be `Authorization`; use `authorizationToken: light-oauth` for that
case.

## Security Configuration

`security-msal.yml` validates Azure MSAL tokens. It is required when the handler
is active.

Example:

```yaml
enableVerifyJwt: ${security-msal.enableVerifyJwt:true}
ignoreJwtExpiry: ${security-msal.ignoreJwtExpiry:false}
enableRelaxedKeyValidation: ${security-msal.enableRelaxedKeyValidation:false}
issuer: ${security-msal.issuer:}
audience: ${security-msal.audience:}
jwt:
  certificate: ${security-msal.jwt.certificate:}
  clockSkewInSeconds: ${security-msal.jwt.clockSkewInSeconds:60}
  keyResolver: ${security-msal.jwt.keyResolver:}
```

Recommended settings:

- Set `issuer` to the Azure tenant issuer when the tenant is known.
- Set `audience` to the SPA client id or the expected Azure access-token
  audience.
- Keep `ignoreJwtExpiry: false` in production.
- Use the configured Microsoft JWK or certificate resolver supported by the
  gateway security runtime.

`security.yml` remains the normal light-oauth verifier. It validates the
light-oauth access token stored in the `accessToken` cookie and provides the
principal used by gateway authorization logic.

## SPA Integration

Initial exchange:

```javascript
await fetch("/auth/ms/exchange", {
  method: "POST",
  credentials: "include",
  headers: {
    Authorization: `Bearer ${azureMsalAccessToken}`
  }
});
```

Subsequent requests with the existing light-oauth authorization pattern:

```javascript
await fetch("/api/orders", {
  credentials: "include",
  headers: {
    "X-CSRF-TOKEN": csrf
  }
});
```

Subsequent requests with the Azure MSAL authorization pattern:

```javascript
await fetch("/agent/chat", {
  credentials: "include",
  headers: {
    Authorization: `Bearer ${azureMsalAccessToken}`,
    "X-CSRF-TOKEN": csrf
  }
});
```

In both patterns, the SPA must send cookies with `credentials: "include"`.
In the Azure MSAL authorization pattern, MSAL.js is responsible for obtaining
and refreshing the Azure access token used in the browser request
`Authorization` header. The gateway is responsible for validating the BFF
session and injecting the light-oauth token into `lightTokenHeader`.

## Logout

Logout clears all BFF cookies managed by the handler:

```text
GET /auth/ms/logout
```

The handler returns an empty `200` response with deletion cookies for the known
session cookie names.

## Error Handling

Important error codes:

| Code | Meaning |
| --- | --- |
| `ERR11000` | Azure MSAL bearer token is missing on the exchange endpoint. |
| `ERR11001` | light-oauth token exchange failed. |
| `ERR10000` | Azure MSAL token or light-oauth token verification failed. |
| `ERR10036` | CSRF token is missing from the request. |
| `ERR10038` | CSRF claim is missing from the light-oauth token. |
| `ERR10039` | Request CSRF and token CSRF do not match. |
| `ERR10052` | Token response does not contain `expires_in` and the JWT has no usable `exp`. |

## Implementation Notes

Rust `light-pingora` and Java `light-spa-4j` use the same token placement
contract:

- `authorizationToken: light-oauth` preserves the existing behavior and injects
  the light-oauth token into `Authorization`.
- `authorizationToken: azure-msal` verifies the request's Azure bearer token
  with `security-msal.yml`, preserves that token in `Authorization`, and
  injects the light-oauth token into `lightTokenHeader`.
- `lightTokenHeader` defaults to `X-Light-Token` and must not be
  `Authorization` when `authorizationToken` is `azure-msal`.

In `azure-msal` placement, the gateway requires the Azure bearer token only
when a BFF session cookie is present. Requests without `accessToken` or
`refreshToken` cookies keep the existing pass-through behavior so public
endpoints are not forced to authenticate at this handler.
