# Native-order supported packaged chain

This profile uses the unchanged qualified bootstrap-only ZIP, not the
Traceability observer executable. It emits a complete configuration source and
deterministic layer, validates both with real offline Rust Init, then sends26
sequential requests through pinned AL2023 RIE with real Init/factories/service
and SDK paths. All dependencies are owned loopback fakes. The package SHA is
pinned in the driver so an instrumented binary cannot be substituted silently.

Run from `/home/steve/workspace/light-fabric-light-lambda` with a fresh output:

```bash
rtk proxy python3 -B apps/light-lambda/tests/l2/composed.py --out /home/steve/workspace/artifacts/implementation/light-lambda/my-composed-chain-rerun
```

The optional `--package` argument must identify the existing ZIP with SHA-256
`24955f85bcc632989f6c8ed772c611dbab5530b8813abc14026beb64a6a47bc4`.
No application rebuild is needed. Config documents are in the output's
`source/`, extracted manifest-checked documents in `opt/`, and the deployable
configuration artifact is `config-layer.zip`. Runtime/dependency observations
are in `runtime/`; results and per-request dependency deltas are in results.json.

## Actual configuration

The `handler.yml` chains preserve the relative order of
`light-lambda-native/src/main/resources/config/handler.yml`, omitting only
Traceability (instrumented proof kept separate) and unavailable Validator:

```yaml
chains:
  request: [metrics, limit, correlation, requestCors, requestHeader,
            requestTransformer, audit, token, router, specification, security, sanitizer]
  response: [responseHeader, responseCors, responseTransformer]
  admin: [correlation, specification, security]
defaultHandlers: [request, proxy, response]
paths:
  - {path: /health, method: get, exec: [health]}
  - {path: /adm/server/info, method: get, exec: [admin, info]}
  - {path: /adm/logger, method: get, exec: [admin, getLogger]}
```

All16 business-chain aliases and the three local terminals are explicitly
declared by their existing native class identities. This profile exercises
Unified Security's JWT branch; it does not assert simultaneous Basic/API-key
branches or select unavailable native catalogue entries.

| Configuration | Supported setting used |
|---|---|
| limit.yml | Configured address buckets:198.51.100.7→100/h,198.51.100.200→1/h; native order before authentication, not identity quotas |
| correlation.yml | Enabled, generation allowed; caller ID and fresh warm IDs verified |
| cors.yml | Allowed origin https://test.invalid, GET/POST; denied origin is distinct |
| header.yml | Request x-request=configured/x-added=request-before/x-remove=request-added. Response x-added/x-remove=response-before, x-chain=composed, removes x-business; configured origin https://headers.invalid is an ordering witness |
| Transformer configs | Enabled only for /x; preserved fixture rule bodies and endpoints merged without modification |
| audit/metrics | Enabled masked stdout Audit with trusted service ID; existing APM-class closed EMF; no network sinks |
| token/client/cache | Client credentials at owned /token; explicit synthetic credentials, zero early renewal/retry delay, one named token cache. Native service_id-presence condition retained |
| router/direct registry | Forwarding enabled, rewrite rules empty; http-service→http://127.0.0.1:19091/base |
| OpenAPI/security | Captured local3.1 spec: /x requires read, /changed requires write; auth issuer test-issuer/audience test-audience. Full primary/secondary signature/scope checks remain enabled |
| sanitizer | Existing full config via values overrides: HTML decoded JSON body, header sanitizing disabled |
| admin terminals | Explicit local GET bindings, existing JWT-only Info/GetLogger gates; no setter/cache activation |

The preserved fixtures are:

- `T-01-metadata-native-cel-multi-contract-v1`: on POST /x with body mutate,
  path→/changed, body→transformed, x-added→updated, x-remove removed,
  query added=7. This precedes Router, Specification, Security and Sanitizer.
- `T-02-response-header-remove-update-multi-contract-v1`: GET /x response
  removes x-remove and sets x-added=after, overriding ResponseHeader's witness.

Original fixtures and Java/Rust expectations are untouched. The composition is
a new local application profile, not a new Java differential/parity claim.

## Ordering and combined behavior proved

| Witness | Required observed result |
|---|---|
| CORS denial plus invalid JWT |403 ERR10092, no OAuth/JWKS/backend calls at cold start |
| Malformed OAuth reply plus invalid JWT |500 ERR14004 before Router/security; no HTTP/Invoke |
| HTTP selected, then invalid JWT or malformed JSON |401 or400, zero HTTP/Invoke; Router selection does not cause eager dispatch |
| POST /x mutate with read-only JWT | Becomes /changed and fails scope403; later checks use transformed path |
| Same mutation with write authorization | HTTP /base/changed/body transformed or SDK /changed/body transformed; SDK query includes added=7 |
| GET /x response stages | ResponseTransformer x-added=after removes earlier x-remove; ResponseCors overrides configured response origin with the request origin |
| SDK FunctionError / HTTP disconnect |502 ERR14014 /400 ERR10089, response x-chain witness retained, Audit/EMF use final posted status |
| Oversized input and invalid JWT |500 ERR14032 response-context replacement wins, zero dispatch; earlier response-header witnesses are discarded by the approved replacement behavior |
| Bad CORS consumes configured quota, then repeats |403 first,429 ERR10088 next: limiter precedes CORS/security |
| Primary/secondary token handling | Signed synthetic OAuth JWT is verified normally. With service_id it is primary if caller auth is absent, secondary if present. Without service_id Token bypasses; absent caller auth is401 ERR10002 |
| Warm requests | A→writer→B→A principals, changed→original paths/body/query, explicit→generated→new correlation IDs, and allowed→absent origins do not contaminate later exchanges |
| Shared admin chain | Exact redacted Info/logger responses,401 with absent token and no OAuth fetch; Health bypasses the business chain |

Two combined behaviors are recorded explicitly, not normalized away:

- Native HTTP Router concatenates host+path without appending the event query
  map. Thus HTTP sees /base/changed while the SDK event retains added=7.
- Audit completion descriptors are prepared for the selected chain, so early
  request-policy rejection still emits a policy Audit record. ResponseCors may
  echo the request Origin on an earlier limiter429 because RequestCors was not
  reached; on its own ERR10092 denial, the rejected Origin is suppressed and the
  configured ResponseHeader origin remains. These are observed composition
  semantics, not a new production-security approval or full native parity claim.

Final profile:26 service requests (1 cold/25 warm),7 signed SDK Invokes,
5 HTTP calls including an unfollowed redirect,2 OAuth fetches,1 JWKS fetch,
22 business Audit records,22 EMF records and26 exactly-once finalizations.
The four local admin requests do not run business Audit/Metrics. No Init-only
case is counted as handler execution. Credentials/tokens are synthetic and
observations contain only allowlisted fields or role booleans.

Source: composed.py, composed_dependencies.py; two new fake privacy/observation
regressions in test_composed.py. Evidence:
`/home/steve/workspace/artifacts/implementation/light-lambda/WP16-composed-chain-20261019T160000Z`.
Earlier89 individual-family requests remain separate, not rerun or double-counted.
Traceability's separate instrumented state proof remains separate. Validator,
Router rewriting, paused precision work and AWS execution remain excluded;
full WP12/WP13 is not claimed.
