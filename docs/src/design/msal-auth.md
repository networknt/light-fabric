# MSAL Auth Handler

## Status

Initial Rust implementation is complete in `light-pingora` and `light-gateway`. It includes config loading (`msal-auth.yml`), standalone Microsoft Entra ID token validation through `security-msal.yml`, double-submit cookie CSRF handling, gateway auth-principal propagation, and downstream `Authorization` injection.

## Purpose

The `msal-auth` module is an alternative to `msal-exchange` for Microsoft Entra ID single-page application (SPA) architectures where the frontend acts as the primary OAuth client.

In this flow:
1. The SPA handles Microsoft authentication, token acquisition, and token refresh directly.
2. The SPA submits the Entra ID access token to the gateway's `/auth/ms/login` endpoint.
3. The gateway validates the Entra ID token with `security-msal.yml` and sets the `accessToken` and `csrf` cookies using the double-submit cookie pattern.
4. On subsequent API calls, the gateway validates the Microsoft JWT with expiry enforcement, compares the CSRF request value to the CSRF cookie, sets the gateway auth principal for later handlers, and forwards the token in the `Authorization: Bearer` header.

This eliminates the need for an internal `light-oauth` token exchange, reducing infrastructure dependencies while maintaining backend API security.

## Configuration

### `handler.yml`

Register `msal-auth` in the handler chain before handlers that need `ctx.auth` or the downstream `Authorization` header, such as `access-control`, `router`, or proxy handling.

```yaml
handlers:
  - msal-auth
  - router

paths:
  - path: /auth/ms/login
    method: POST
    exec:
      - msal-auth
  - path: /auth/ms/logout
    exec:
      - msal-auth
  - path: /**
    exec:
      - msal-auth
      - router

defaultHandlers:
  - msal-auth
  - router
```

### `msal-auth.yml`

```yaml
enabled: ${msal-auth.enabled:true}
loginPath: ${msal-auth.loginPath:/auth/ms/login}
logoutPath: ${msal-auth.logoutPath:/auth/ms/logout}
cookieDomain: ${msal-auth.cookieDomain:localhost}
cookiePath: ${msal-auth.cookiePath:/}
cookieSecure: ${msal-auth.cookieSecure:false}
sessionTimeout: ${msal-auth.sessionTimeout:3600}
cookieSameSite: ${msal-auth.cookieSameSite:None}
```

### `security-msal.yml`

`msal-auth` requires `security-msal.yml` when the handler is active and `msal-auth.enabled` is true. The config is loaded independently from the normal `security.yml` runtime.

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

- **Login (`/auth/ms/login`)**: Expects an Entra ID token in the `Authorization: Bearer` header. Validates it using the `security-msal` runtime with expiry enforcement. Generates a secure CSRF token and returns both `accessToken` and `csrf` as `Set-Cookie` headers.
- **Logout (`/auth/ms/logout`)**: Clears the `accessToken` and `csrf` cookies and returns a success response.
- **Session Validation (any path with cookies)**: Reads the `accessToken` cookie. Validates the JWT with expiry enforcement. Checks that the CSRF request value matches the CSRF cookie. If valid, it sets the gateway auth principal and forwards the `accessToken` downstream in the `Authorization: Bearer` header.

## Double Submit Cookie CSRF

Because an Entra ID token cannot be minted with a custom CSRF claim by this gateway, `msal-auth` enforces CSRF protections using the double-submit cookie pattern. The SPA reads the generated `csrf` cookie and submits it back.

The CSRF value is accepted from the following sources, in order of precedence:
1. `X-CSRF-TOKEN` header.
2. `Sec-WebSocket-Protocol` value starting with `csrf.` (when the request has `Sec-WebSocket-Key` and `Sec-WebSocket-Version`). This provides specialized CSRF support for Websocket upgrades since browser WebSockets cannot send custom HTTP headers.
3. Query parameter `csrf`.

The gateway compares the value from one of these sources against the `csrf` cookie. If they match, the session is validated.

## Refresh Flow

Unlike `msal-exchange` or `stateless-auth`, `msal-auth` does not issue or manage refresh tokens. The SPA is responsible for using MSAL.js to silently refresh the Entra ID token and calling `/auth/ms/login` again to update the session cookies before they expire.

## Reload Behavior

The gateway reloads `msal-auth` when `handler.yml`, `msal-auth.yml`, or `security-msal.yml` changes. Reloading `security-msal.yml` refreshes both `msal-auth` and `msal-exchange` because both handlers validate Microsoft tokens with that security runtime.
