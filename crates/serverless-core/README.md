# Serverless core (WP7 and WP9a)

Provider-neutral invocation types, compatibility registry, immutable plan compiler,
pipeline, status bytes, bounded blocking-work coordination, and transport-neutral
JWT/JWT-only unified-security handlers. There are no provider codecs/clients,
Lambda runtime protocol, or conformance runner here. Security tests use the real
shared verifier against loopback JWKS; external dependencies and dispatch are fake.

`ExecutionPlan::compile` accepts effective handler configuration after substitution
and audit configuration separately. It supports array/JSON-string/comma-list handler
declarations, append-only additional handlers/paths, additional-chain overrides,
nested chains, chain-before-alias lookup, and Java's later alias replacement.
Disabled known handlers must be explicitly disabled in `Registry`; a missing
factory is an Init error, never a successful no-op. Deferred classes remain errors
when enabled even if a factory is supplied. Enabled factories build once per alias.
Other audit policy fields are validated by the eventual audit factory; core owns
only `auditOnError` and `auditStackTrace` and rejects handler-owned aliases.

Java `Handler.getChain` does not use `handler.basePath`; core retains it without
prepending or stripping it. OpenAPI handling is a later handler responsibility.
Methods are case-insensitive. Paths are already decoded by the ingress adapter:
core does not decode them again. Literal segments precede parameters, then longer
matches precede wildcards (approved R-01a). Captures populate path parameters while preserving the normalized query multimap;
absent maps are represented by empty collections. The WP8 v1 adapter copies those
captures into the scalar query map, overriding same-name values there only (R-03).
Target allowlist validation is owned by serverless-aws.

Factories declare authentication capabilities from effective configuration.
Conditional/skip-path authentication cannot claim an unconditional identity
guarantee. `RequestCapabilities::for_limit` classifies client/user/jwt keys and JWT
resolvers. Every flattened path/default plan is checked, and execution checks for
a verified principal before entering identity-keyed limits. Real authentication
capability derivation and resolver validation remain WP9a/WP12. Alias spelling
does not establish authentication. The trusted `Principal::from_verified`
constructor has no deserialization/header counterpart. WP9a must convert a
successful `light_security::AuthPrincipal` at its handler boundary: a direct
core dependency would pull `aws-lc-rs` through TLS and violate the literal `aws-*`
dependency restriction. This constructor is a trusted Rust-code boundary, not a
cryptographic proof that can constrain malicious factory code.

The compiler requires exactly one enabled terminal in every executable chain and
request-before-terminal-before-response ordering. A terminal is either configured
target dispatch or a local handler that must Respond/Reject. Request Respond/Reject
short-circuits all remaining request work and dispatch. Ordinary errors get response
policies. Handler errors/panics fail closed as ERR14004; response failures discard
partial output and stop response processing. There is no retry or redispatch.

The caller supplies both absolute deadlines. The executor does not subtract reserves
or restart budgets. It bounds request plus dispatch, cancels on expiry, and retains
an independent cancellation signal and deadline for bounded response error treatment.
Response expiry returns minimal ERR14017 without rerunning response handlers.
Completion callbacks run once in reverse order; removal/repeated-finalization
rejection are explicit. Dropping an executing future performs bounded synchronous
finalization even though an aborted caller can no longer receive an HTTP response.
Finalizers must not await, perform network I/O, or block without bound. Concrete
audit/metrics emission is deferred; core records completion and dispatch observations.

`BlockingExecutor::process()` owns one process-wide pool with four permits, a 1 MiB
input cap and 100,000-step cap. These are internal mechanism caps, not customer
rule configuration or qualified latency budgets. The executor owns the step loop;
each trusted step must have its own finite execution bound. WP12 must reject
actions without enforceable bounds and validate bounds against the response reserve.
This is not a sandbox for arbitrary code or unbounded steps. Inputs/work/output are
owned, and implementations must not capture exchange/shared response state. The
permit moves into the closure and survives caller timeout. Tokio cannot terminate
running blocking work; late owned results are discarded. Tests use bounded finite
wall-clock work; async deadline tests use paused Tokio time.

`StatusError::to_bytes` intentionally reproduces Java Status.toString ordering and
unescaped string fields, including the WP2 invalid-JSON sample. Metadata key order
is explicit. Only trusted status templates belong here; backend error text and
credentials must not be interpolated. Custom Java serializers are unsupported.
ERR14017 is allocated from the checked ERR14010–ERR14019 block; other entries remain
reserved for their later packages. No existing status YAML is overwritten.

The dependency test walks resolved transitive closures, including build/dev edges.
It checks existing core/AWS/runtime crates and reports absent gateway-policy/app
checks as deferred. It never creates a fake app or treats absent checks as executed.
Exactly `aws-lc-rs` and `aws-lc-sys` are permitted cryptographic dependencies in
the core. Actual AWS SDK/configuration packages, `lambda_runtime*`,
`light-lambda-runtime`, `serverless-aws`, and Pingora remain forbidden.

## WP9a security boundary

`handlers::jwt::JwtHandler` uses the existing shared runtime and
`verify_jwt_token_for_services_with_deadline` with the original invocation
deadline. It adds no token decoder, cryptography, cache, or JWKS gate. Only
successfully verified and authorized `AuthPrincipal` output reaches the private
adapter into the existing core `Principal`. Denials and skip paths clear the
principal. The shared type, APIs, cache and gate are unchanged under the owner's
scoped disposition of the original CRITICAL lookup.

Required scopes are trusted handler-construction input, explicitly approved for
WP9a. No YAML required-scopes key or OpenAPI extraction is introduced. Enabled
scope verification uses exact any-of membership in string/list `scope` or `scp`;
empty requirements permit the specification-free case. Codes distinguish missing,
invalid, expired and scope-mismatched credentials. A supplied `X-Scope-Token` is
verified and selects the scope source even when primary scopes already match;
it never replaces the verified primary principal. Empty/null `scope` can use `scp`.
Because the shared API hides
expiry details, generic JWT-validation denials alone are diagnosed through a
second shared Ignore-expiry verification. Its verified output never authenticates
the request; both calls share the original deadline. Key-resolution/dependency
failures do not trigger that pass. Deadline errors retain `504/ERR14017`; ordinary
JWKS failures map to the approved gateway denial.

Unified security preserves Java's configured order and literal `startsWith`
matching, checking anonymous prefixes first. Disabled-factor entries continue
the search; the first enabled matching JWT entry authenticates. An unmatched
path returns `400/ERR10078`; Basic on a JWT-only prefix returns the approved
`401/ERR10000` difference. No Basic challenge is added to JWT-only denials.
Enabled Basic/API-key/SWT/SJWT/LDAP or unknown/malformed factors fail Init,
including unused entries. No Pingora session adapter is used.

Init validation is Light Lambda policy; shared callers keep their configuration
rules. Skew must be an integer in 0..=60 without override. Production rejects
expiry/mock/token-log switches unless the process has exactly
`LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG=1`. The explicit mock test exception
permits an unauthenticated skip with no principal or JWKS bootstrap, never
fabricated claims. Token logging remains disabled even under the exception.
Unknown keys, enabled SWT, relaxed key validation, h2c enablement, providerId,
nondefault deferred SWT headers, client/user/scope logging, claim pass-through,
and certificate/custom key resolvers are rejected. Default `JsonWebKeySet` and
empty certificate declarations are accepted through shared configuration loading.

These are L0 real-authentication component tests, not WP10 production-app/full L1,
WP13 fixture replay/comparison, AWS qualification, or deployed parity proof.
OpenAPI, policy extraction and remaining mechanisms stay in WP12.
