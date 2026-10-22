# SPA token trust and renewal

Rust SPA authentication uses configuration-owned RS256 authorities rather than
the generic `security.yml` key providers or principal caches. Issue
[#441](https://github.com/networknt/light-fabric/issues/441) builds on the
[session-bound CSRF contract](../../design/msal-auth.md#session-bound-csrf-440).

## Trust roles

| Role | Consumers | Required policy |
| --- | --- | --- |
| `internal` | Stateless/social login, exchange, session renewal and logout | Exact light-oauth issuer/application audience, signature, lifetime, user identity and signed CSRF claim |
| `microsoftId` | Microsoft ID-token subject at `/auth/ms/exchange` | Tenant-specific issuer/version, SPA client GUID as audience, subject, lifetime, nonce presence, no delegated `scp` |
| `microsoftApi` | Additional Microsoft token in `authorizationToken: azure-msal` mode | Tenant-specific issuer/version, API audience, subject, delegated scopes and allowed `azp`/`appid` client |

The internal role requires a nonempty `sub`, `user_id` or `uid`, plus an opaque
signed `csrf` claim. If `token_use` exists it must be `user` or `access_token`;
Microsoft ID-token nonce claims and other explicit purposes are rejected.
ID/API policies reject conflicting explicit `token_use` values. These checks run
on every verification, including warm-key requests. Audience arrays must contain
exactly the one configured application audience. `clockSkewSeconds` accepts 0–300 seconds; a native typed
policy defaults to zero, while the shipped template explicitly uses 60 seconds.
RS256 is mandatory; HS256,
unsigned tokens and generic key/cache fallback are unavailable.

The Microsoft ID-token nonce is a purpose marker supplied by the existing client
flow. The gateway does **not** create or verify an authentication nonce challenge.
Do not describe this flow as providing gateway nonce-challenge binding.

Entra v2 API audiences use the API application's GUID. For v1 the audience may
be its application-ID URI; configure the v1 issuer ending in `/<tenant>/`, and
the policy checks `ver: "1.0"` with `appid`. V2 uses `/<tenant>/v2.0`, `ver: "2.0"`
and `azp`. Microsoft ID and API audiences are independently configured even if
the deployment deliberately uses the same application GUID for both.

Direct `msal-auth` retains its existing dedicated tenant/audience/scope/client
policy in `msal-auth.yml`. It shares the bounded RS256 lookup implementation,
not the exchange subject policy. Its `security-msal.yml` supplies clock skew,
not signing keys or claim-policy overrides. Generic `security.yml` still applies
to independent security handlers elsewhere in the gateway chain.

## Configuration and startup

Install the shipped `apps/light-gateway/config/spa-token-trust.yml` template
beside the other gateway configuration files. Supply values through the normal
values/environment/configserver substitution mechanism. For example:

```yaml
spa-token-trust.internal.issuer: https://oauth.example.com
spa-token-trust.internal.audience: portal
spa-token-trust.internal.jwksUri: https://oauth.example.com/oauth2/keys
spa-token-trust.internal.jwksCaPaths: []
spa-token-trust.internal.clockSkewSeconds: 60

spa-token-trust.microsoftId.issuer: https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/v2.0
spa-token-trust.microsoftId.tenantId: 11111111-1111-1111-1111-111111111111
spa-token-trust.microsoftId.audience: 33333333-3333-3333-3333-333333333333
spa-token-trust.microsoftId.jwksUri: https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/discovery/v2.0/keys

spa-token-trust.microsoftApi.issuer: https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/v2.0
spa-token-trust.microsoftApi.tenantId: 11111111-1111-1111-1111-111111111111
spa-token-trust.microsoftApi.audience: 22222222-2222-2222-2222-222222222222
spa-token-trust.microsoftApi.jwksUri: https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/discovery/v2.0/keys
spa-token-trust.microsoftApi.requiredScopes: [access_as_user]
spa-token-trust.microsoftApi.allowedClientIds: [33333333-3333-3333-3333-333333333333]
```

Replace example authorities and application IDs with independently reviewed
provider settings. There is no default trusted issuer or key endpoint. Stateless
and social handlers need only `internal`. Exchange also needs `microsoftId`;
`microsoftApi` is mandatory only in azure-msal mode. Disabled/inactive handlers
do not require their unused policies. An enabled handler fails startup when a
required role or its CA configuration is invalid.

`jwksCaPaths` contains absolute paths to PEM CA bundles. Additional roots extend
the client's default public roots, apply only to that source, and never inherit
`client.yml` TLS relaxations. Direct MSAL uses `msal-auth.jwksCaPaths` for the same
purpose. All JWKS URLs must be administrator-configured HTTPS endpoints without
credentials or fragments; redirects are disabled. Token-controlled `jku`, generic
certificate routes and unrelated authorities with a colliding `kid` cannot select
another signing source.

The `spa-token-trust` diagnostic module is restart-only. Its targeted reload
reports `requiresRestart`. A supported full handler-runtime replacement also
constructs fresh sources before installing each immutable handler snapshot.
Neither replacement carries over cached keys, misses, failure state or principals.
Direct-MSAL replacements validate CA material before publishing diagnostics.
Rust embedders must pass an explicit `SpaTokenTrust` to `SpaSessionRuntime::new`;
the former generic `SecurityRuntime` constructor argument is no longer accepted.
`MsalExchangeRuntime::new` takes an ID-role trust and an optional API-role trust.
Constructing one role with another role's trust cannot bypass verification.
Within one bundle, roles share retrieval only when endpoint and CA path settings
are identical; their claim policies always remain separate. Separate bundles do
not share global signing or principal state.

## JWKS availability and lookup evidence

Successful response freshness is bounded to 30–300 seconds. `no-cache`, `no-store`,
`max-age=0` and shorter max-age values have an explicit 30-second floor. After
freshness expires, known keys remain available for at most one additional hour
while a refresh runs or the source is unavailable. Cached known-key verification
does not wait for unknown-key network work or its cooldown. A successful refresh
discards superseded keys immediately for subsequent lookups.

This availability window can delay emergency revocation during an outage by up
to freshness plus one hour. Token expiry and claim policy remain enforced. Use a
configuration replacement/restart when emergency key removal must discard local
state immediately; do not rely on the outage retention window for revocation.

Refresh uses a single flight, at most 32 concurrent lookup slots, a 500 ms bounded
wait for an occupied flight, five-second HTTP/connect limits, a 1 MiB response
limit and at most 512 source keys. No key-state lock is held during network I/O.
Cancellation of an initiating request does not abandon the owned refresh task.
Failed lookups have a five-second cooldown.

Unknown keys have a 30-second successful-refresh reprieve. Skipping a fetch is
uncertainty (`503`), not proof that the key does not exist. Only a new successful
fetch can confirm a missing requested key. That specific negative result is
terminal (`401`) for 30 seconds, in a cache limited to 256 IDs. Duplicate eligible
signing keys are terminal for that key. Known-key invalid signatures and failed
claim policies are terminal; same-`kid` rotation can be accepted only when a
permitted new fetch proves the replacement key. These failures do not rotate
refresh tokens indefinitely behind `503` responses.

Publish public keys before signing with them and allow cache propagation time.
Prefer distinct rotation IDs. A freshly confirmed missing key requires corrected
provider publication and may require a new login.

## Issuance, renewal and recovery

Every returned internal JWT is verified with the selected internal authority
before scopes/identity are accepted, Authorization is injected, or access/CSRF
cookies are installed. Cookie CSRF must match the signed claim. Ordinary renewal
preserves the verified CSRF context. Unverified request/cookie values are never
promoted into a new signed session.

If OAuth rotates a refresh credential but signing verification is temporarily
uncertain, the response retains **only** the new opaque HttpOnly refresh cookie.
It never installs unverified access, CSRF or identity cookies. Pending refresh
cookies use the shorter configured session/remember lifetime; unverified remember
metadata cannot extend persistence. A bounded pending
grant cache also aliases the rotated credential to the verified-CSRF context,
or to the separate refresh-only recovery context, so short retries can verify
the same grant without immediately rotating again.

| Renewal outcome | Response and dispatch |
| --- | --- |
| Newly issued JWT and binding verified | Install matching cookies and use the verified new token |
| Temporary uncertainty; original verified token still valid after the attempt | Normal downstream success with original token and any rotated refresh cookie; no `Retry-After` |
| Temporary uncertainty; original expired or absent | `503`, `Retry-After: 1`, retain existing state and any rotated refresh cookie; do not execute original request |
| Confirmed missing key, invalid signature, failed claim/binding policy or terminal grant rejection | `401`; no newly issued credentials installed |

Expiry is rechecked after waiting. An expired original token is never used for
fallback. The expired-token exception is used only to authenticate a renewal or
logout context; it does not authorize API dispatch.

Refresh-token-only recovery remains disabled by default, preserving #440's
fail-closed policy. To enable it deliberately, configure exact canonical origins
in `statelessAuth.recoveryOrigins` or `msal-exchange.recoveryOrigins`. Recovery
requires POST, an exact trusted browser `Origin`, bounded unambiguous cookies,
and a matching `X-CSRF-TOKEN` header. Cookie/header equality is **not** treated as
authenticated session binding: recovery generates a new random nonce, verifies
the returned signed JWT and never dispatches the original operation.

Successful refresh-only recovery returns `409` with code `SPA_SESSION_RECOVERED`
and matching signed session cookies. It means the original operation was not
executed; a client must explicitly retry using the updated cookie context.
Temporary uncertainty returns `503` as above. Do not enable recovery before the
client understands these outcomes. No Portal change is included in this issue,
and automatic replay of user operations is not introduced.

Enforced exchange/stateless logout verifies the same internal policy and signed
CSRF claim, with expiry ignored only for cleanup. Direct-MSAL logout retains
#440's HMAC proof of previously verified exact access-token bytes. No-cookie
logout is idempotent; malformed/partial cookie sets fail closed. Explicit logout
observation-only mode remains unprotected and must be removed at cutover.

The existing Rust Google/social path exchanges the provider subject and verifies
the returned internal JWT before creating the login response. There is no local
Google account-link side effect in this handler to authorize from unverified
claims; this implementation does not invent one.

## Migration and qualification

1. Inventory the actual light-oauth issuer, application audience, RS256 signing
   endpoint and session claims. Configure its dedicated internal policy; do not
   copy generic relaxed-key or verification-disable settings.
2. Configure Microsoft ID and API policies separately. Confirm the SPA sends an
   ID-token subject to exchange and a delegated API token in azure-msal mode.
   Stop accepting ID/API purpose confusion before requesting new internal tokens.
3. Install the new template and CA bundles on all replicas. Validate required
   selected roles at startup in an isolated environment, then replace snapshots
   coherently. Policies cannot fall back to legacy generic security configuration.
4. Keep refresh-only recovery disabled unless clients handle explicit `409`/`503`
   outcomes and the exact Origin allowlist is provisioned. Preserve #440's
   WebSocket protocol and Origin configuration and avoid automatic user-request
   replay.
5. Qualify login/exchange, HTTP, logout, Chat and Controller through the deployed
   browser/proxy with real provider tokens. Local source/component/loopback tests
   are separate from live Entra, production CA/replica behavior and remote CI.

Relevant local commands (run from the repository root):

```sh
cargo test --locked -p light-pingora -p light-gateway
cargo check --workspace --all-targets --locked
cargo fmt --all --check
mdbook build docs
```

The opt-in public TLS probe performs no authentication or deployment. It verifies
that adding a local CA still permits the public Entra JWKS TLS endpoint:

```sh
cargo test --locked -p light-pingora --lib spa_jwks::tests::additional_ca_preserves_default_public_roots -- --ignored --exact
```
