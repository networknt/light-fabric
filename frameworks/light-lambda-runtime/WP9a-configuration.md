# WP9a configuration boundary and WP10 additions

Runtime Init loads `security.yml`/`client.yml` through the existing shared
configuration path and installs real core factories for the Java JWT and unified
security class identities, independent of deployment alias spelling. Both
security files are checked for unsupported/unknown settings before bootstrap or
business-client construction. A selected enabled JWT factor cannot use a disabled
JWT verifier. Explicit custom factories remain trusted embedding inputs; the new
qualification uses the real default factories.

The app-owned templates are absent: WP9a does not create `apps/light-lambda`.
WP10 must add `handler.yml`, `lambda-app.yml`, `lambda-proxy.yml`,
`light-lambda.yml`, `security.yml`, `unified-security.yml`, and `client.yml`, with
the Java file names/placeholders and shared module defaults. In particular:

- JWT verification defaults on; production expiry/mock/token-log switches default
  false, and `security.jwt.clockSkewInSeconds` defaults 60 and must be <=60.
- Include `jwksRefreshMinIntervalSecs: ${security.jwksRefreshMinIntervalSecs:30}`
  from WP6. Verification uses shared cache/bootstrap/provider settings and the
  shared client's timeout/TLS configuration. It does not fetch OAuth credentials.
- JWT scope requirements come only from trusted handler construction in WP9a;
  there is no new YAML key. OpenAPI-derived requirements wait for WP12.
- Unified templates use `enabled`, `anonymousPrefixes`, and ordered
  `pathPrefixAuths` with `prefix`, `jwt`, and optional `jwkServiceIds`. Unsupported
  factors must be false/absent; enabling one fails Init with "not supported until
  WP12". Disabled factor-specific service settings must remain at empty defaults.
- For all other Java default-chain handlers, supply their disabled configuration
  and explicit `HandlerRegistration { factory: None, ... }` entries. Disabled
  registrations are accepted; enabled unimplemented ones fail Init. No default
  alias is converted into a successful no-op or marked implemented prematurely.

The explicit insecure test exception is exactly
`LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG=1`. It never relaxes the skew limit or
enables token logging. Mock mode permits an anonymous test bypass with no
fabricated principal. The component test invokes isolated child test processes
with that key removed or set exactly to 1, preserving the parent's environment.

Actual core-pipeline denials are tested with a counting fake backend. Runtime
component tests execute ALB decode, default real security factories, shared
verification and v1 authorizer construction, alternating users/denials/public
requests. They are not the built production binary/full L1 harness, WP13 fixture
runner/comparator, or AWS qualification. SDK-only response/error posting and the
approved L1-08 behavior are unchanged.
