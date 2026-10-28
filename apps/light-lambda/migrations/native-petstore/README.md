# Native pet-store reconciliation and Validator qualification

This is a public checked-in sample, not a customer configuration. The delivered
bootstrap ZIP is unchanged. The original native source/configuration and prior
evidence remain unchanged.

## Reconciled native configuration

Generate separate configuration candidates and record their real delivered-app
offline Init results, from the worktree:

```bash
rtk proxy python3 -B apps/light-lambda/tests/l2/native_reconciled.py --out /home/steve/workspace/artifacts/implementation/light-lambda/petstore-reconciled-$(date -u +%Y%m%dT%H%M%SZ)
```

`reconciled-full/` preserves every original request/response/admin chain and path
binding, including unavailable handlers. It resolves the sample's handler list
and adds these missing declarations, using native class names:

- `middleware.cors.RequestCorsMiddleware@requestCors`
- `middleware.cors.ResponseCorsMiddleware@responseCors`
- `cache.CacheExplorerHandler@cache`

Each suffix has the prefix `com.networknt.aws.lambda.handler.`. Native
`Handler.initChains` rejects unknown aliases; there is no implicit alias
registration. Missing declarations and missing implementations are distinct.
Native LoggerSet and CacheExplorer are implemented Java handlers; Rust has not
implemented their application profiles. The full candidate retains them and
fails Init (`logging.yml: logger SET unsupported` first), rather than silently
removing admin routes. SWT remains explicitly unavailable as well.

The OpenAPI server URL is `https://petstore.networknt.com/v1`. Native
OpenApiMiddleware uses `ApiNormalisedPath(requestPath, helper.basePath)`, and
Validator uses the same base path. Proxy routing uses the original request path.
Consequently `/v1/pets` is the intended native ingress and `/pets` is its
normalized specification path. The earlier report's claim of a path mismatch
was too broad: the server prefix reconciles the two. The new candidate preserves
both the original document and the original prefixed function keys. Neither a
Router rewrite nor a changed schema path is needed. Unprefixed paths also remain
unchanged by the existing normalizer; this is not a prefix-enforcement policy.

The native OpenAPI declares PUT `/pets/{petId}`, but its function map has no PUT
binding. The full candidate preserves that omission explicitly. It does not
invent a target. All cloud function names are replaced by `test-only-business`;
no cloud request is sent.

Other local dependency and policy settings reuse `composed.prepare`: owned
loopback Invoke/HTTP/OAuth/JWKS, synthetic credentials, masked stdout Audit/EMF,
configured limiter buckets, header/CORS ordering witnesses, Transformer fixtures
restricted to `/x`, JWT issuer/audience/scope settings and HTML Sanitizer.
These are test adaptations, not original native deployment values. Template
extension/audit/base-path placeholder options omitted by the builder retain
their empty/false/root defaults. Source hashes are recorded; no truststores,
customer data, raw credentials or tokens are copied from native values.yml.

## Validator-enabled qualification profile

The files beside this README form the minimal profile copied to
`validator-qualification/`: Specification → Validator → Proxy. This is explicitly
an isolated Validator integration profile, not the full chain or admin migration.
Security is off for this isolated semantic test and zero-JWKS assertion; Validator
is **on**, body validation is on, and skip prefixes are empty. The complete
original public OpenAPI is retained byte-for-byte as `openapi.yaml` (SHA-256
`012efdf3682d893f7c473738d158615f2f0a293d988765ef4676eb3307c46101`).
Only POST `/v1/pets` is bound to the owned fake. Dynamic Runtime API/fake Invoke
addresses are supplied by the existing built-app harness.

The ordinary delivered bootstrap must reject this profile because its default
precision-engine admission is still closed. No runnable layer is advertised
from these rejected configurations. Offline precision preparation must remain
fail-closed without spawning a child or using numeric fallback.

The existing named ignored test
`validator_profile::real_precision_child_built_application_valid_invalid` now
compiles the following acceptance sequence through the explicit qualification
application, real Init/factory/service/SDK and actual precision engine:

1. POST `/v1/pets`, `{"name":"Tucker"}` → ALB200, one Invoke.
2. Same path, `{}` → ALB400 `ERR11004` / `VALIDATOR_SCHEMA`, zero Invokes.
3. Same warm application, `{"name":"Milo"}` → ALB200, one Invoke.

Assertions require exactly two backend calls with those two valid bodies,
unchanged `/v1/pets` forwarding and zero JWKS fetches. Required traceability
header and JSON media type are present so `{}` tests the schema, not missing
header/media admission. Existing actual-child owner identity, shutdown, exit and
reap instrumentation covers success/assertion/timeout cleanup paths.

**Not executed:** platform clearance remains unconfirmed. The existing candidate
bundle/resource/failure-policy approvals are retained, not reopened. There is no
new production-impact approval request in this change. The approved resource
values remain experimental limits for the named local qualification only.
Default activation, immutable production bundle loading, semantic/resource
qualification and the old v6 binary's reproducibility gap remain open. Paused
worker experiments are untouched. Router rewriting is separate.

Permitted compilation and non-child tests:

```bash
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --test native_petstore
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --test validator_wiring
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --test l1 --no-run
rtk proxy env CARGO_INCREMENTAL=0 cargo build --locked --offline -p light-lambda --example validator_profile
```

The application example is a separately compiled local qualification executable,
not a replacement bootstrap ZIP. Do not run the ignored real-child test until
separate platform clearance is confirmed. Scripted IPC wiring tests and metadata
tests do not satisfy its acceptance target.
