# Per-alias migration and local package qualification

Consolidated package/configuration/build/rerun handoff:
[DELIVERY-HANDOFF.md](../../DELIVERY-HANDOFF.md).

All23 aliases below are registered/admitted within package-profile restrictions.
**22 have observable default-package/RIE proof. Traceability now has separate
[instrumented-only state proof](TRACEABILITY-OBSERVER.md); its default-package
observation remains blocked.** Selecting
a handler, accepting Init, preserving a header or passing a component fixture
is not automatically package qualification. Shared profile requests are counted
once (53 additional +36 retained =89), not once per alias in this table.

| Alias | Component/built-app evidence, separate from L2 | Package/RIE profile and scope | Current status |
|---|---|---|---|
| proxy | Real SDK/ALB baseline and built-app errors | Baseline, original profiles, additional profiles | Qualified within tested contracts |
| correlation | Selected Q comparisons, scalar/multimap built-app regressions | headers: first scalar, multimap, generation, warm IDs | Qualified |
| requestHeader | Head/pipeline/header tests and selected comparisons | headers: global/prefix update/removal | Qualified |
| responseHeader | Response/pipeline/header tests and selected comparisons | headers: update/removal on successes/errors, no prefix leakage | Qualified |
| jwt | Real JWT/JWKS/cache/scope built-app tests | jwt: valid identities, missing/forged/issuer/audience/expiry; one cached fetch | Qualified, HTTP synthetic JWKS only |
| security | Unified-factor and operation-domain tests | jwt's separate security chain | Qualified for tested JWT branch; no SWT/LDAP |
| router | Built-app forwarding, five retained differential comparisons, failed-state component | router: HTTP methods/body, fallback, errors, redirect/no double dispatch | Forwarding qualified; rewriting unavailable |
| requestTransformer | Real factories/combined-profile/built-app tests; selected T comparisons | transformers: request body mutation, input rejection/no dispatch, warm recovery | Fixed admitted profile qualified |
| responseTransformer | Response factory/output/rollback/built-app tests; selected T comparisons | transformers: response mutation, overflow/error/recovery | Fixed admitted profile qualified |
| metrics | Component/L1 closed-EMF tests | telemetry: exact closed fields, trusted templates/unmatched, final status/error counts | Newly qualified APM-class→EMF profile, no network APM/Influx sink |
| limit | Component/L1 identity quota and traversal tests; retained L comparisons | limit: two address buckets, exhaustion429, reset and warm independence | Newly qualified configured address profile; headersAlwaysSet=true unavailable |
| traceability | State/isolation and span-bridge components; selected Q comparisons | Separate test-only observer package: eight service requests prove actual state, backend-error/warm behavior and disabled-handler control; one ingress rejection is admission only | **Instrumented-only proof passed; default formatter still suppresses span fields** |
| requestCors | Native preflight/body/dispatch component and L1 | cors: preflight200/one Invoke, denial403/zero Invoke | Newly qualified |
| responseCors | Native/pipeline/L1 response tests | cors: allowed echo, suppressed denial echo, no warm-origin leakage | Newly qualified |
| audit | Actual stdout masking/status/occurrence L1 tests | telemetry: five policy records, masking/status/trusted identity, warm absence | Newly qualified unsaturated stdout profile; best-effort delivery unchanged |
| token | Real provider/cache/error/secondary-header L1 | token: denial/malformed/recovery/cache and prefix isolation | Newly qualified client-credentials profile |
| specification | Captured local resources, declaration order, scope and L1 tests | specification:403/200/401/404/405, mounted source mutation after Init | Newly qualified local OpenAPI resource/scope profile; not Validator |
| basic | Factor/config/L1 credential and path tests | factors/basic: success, wrong/missing credentials, path denial, warm recovery | Newly qualified local Basic profile; no LDAP |
| apikey | Factor/config/L1 tests | factors/apikey: success, wrong/missing key, warm recovery/unprotected bypass | Newly qualified plain local-key profile |
| sanitizer | Encoder goldens, body/header components, malformed-body L1 | sanitizer: base64 HTML JSON mutation, malformed400, plain-body/warm behavior | Newly qualified selected HTML body profile |
| health | Exact local response/zero-dispatch L1 | admin: exact GET, reserved POST404 and warm recovery | Newly qualified local terminal; no downstream checks |
| info | Verified-JWT/redaction/exact-response L1 | admin: exact modules, protected-chain401, warm authenticated recovery | Newly qualified existing JWT-only redacted profile |
| getLogger | Installed-filter/fallback/JWT L1 | admin: exact info filter, protected-chain401, reserved POST404/warm recovery | Newly qualified read-only JWT profile; no setter |

Earlier native expectation/comparison inventory is **separate**:373 Rust
expectation passes,362 differential passes,17 single-runner passes and five
forwarding comparisons are retained historical evidence, not rerun here.
Those are selected-fixture counts, not whole-handler conformance. Original
D-08/H-04 and Java L-02 failures remain unchanged and visible in that inventory.

Four unavailable aliases remain outside these23: Validator, SWT, SetLogger,
Cache. Nonempty Router rewrite rules and unsupported network metrics sinks also
remain unavailable. Init rejection demonstrates admission only. Precision-child
qualification/platform clearance, paused work, v6 reproducibility, full
WP12/WP13 and AWS qualification remain open.

Commands: [original profiles](PROFILES.md), [additional profiles](REMAINING-PROFILES.md).
The [native-order composed profile](COMPOSED.md) separately passes26 combined
service requests through the unchanged package. These exercise16 business
aliases plus the three supported local terminals, with ordering/error/dispatch
and warm-isolation witnesses. They do not replace individual-alias evidence or
include Traceability instrumentation.
Family evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP16-remaining-profiles-20261019T140000Z`.
Composed evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP16-composed-chain-20261019T160000Z`.
Traceability instrumented evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP16-traceability-observer-20261019T150000Z`.

## Source-only partial X-04 update (2026-10-20)

The delivered ZIP and its23-alias migration inventory above are unchanged. New
source implements only an opt-in ROOT INFO/DEBUG/TRACE LoggerSet, with real
subscriber/factory/Init/service tests and native POST /adm/logger → truthful GET
built-app qualification. It is not package/RIE-qualified;
CacheExplorer and general LoggerSet names/levels remain deferred. See
[LOGGERSET.md](../../LOGGERSET.md) for exact bounds and the qualified approved
HIGH guard patch. Do not count this as a24th delivered/RIE-qualified alias.

## New local release qualification (2026-10-21)

The original ZIP and its historical22-alias RIE evidence above are preserved.
New ordinary ZIP SHA-256
`d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a`
is under `WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip`.
It is reproducible from two independent clean builds and has these fresh proofs:

| Aliases/profile | New-ZIP evidence |
|---|---|
| proxy baseline | 8 events,7 Invokes,0 JWKS; success/backend/ingress errors and warm isolation;4 additional Init-negative inputs |
| metrics, limit, correlation, requestCors, requestHeader, requestTransformer, audit, token, router, specification, security, sanitizer, proxy, responseHeader, responseCors, responseTransformer, health, info, getLogger | 26-request native-order composed profile;7 Invoke,5 HTTP,2 OAuth,1 JWKS; ordering, combined errors, rejection, warm state and policy assertions |
| jwt and conditional setLogger/getLogger | 24 service requests,zero Invokes,2 JWKS across enabled/fresh processes;2 Init-only gate/config rejections; native POST/GET, schema/auth/size/method/privacy/Audit/reset assertions |
| basic, apikey | Original-ZIP family and component/comparison evidence retained; no new-ZIP family rerun claimed |
| traceability | Instrumented-only state proof remains separate; no new ordinary formatter proof |

This is20 newly exercised admitted aliases plus conditional setLogger, not a
claim that all23 admitted aliases or the whole native sample have fresh new-ZIP
RIE qualification. Startup opt-in, enabled resolved logging, scalar filter,
verified-JWT admin authorization and bounded ROOT INFO/DEBUG/TRACE remain required.
The layer tool's explicit --allow-logger-set validation flag does not grant its
own runtime startup permission. The original ZIP still lacks mutation support.

Exact commands, release/layer identities, old failed-run dispositions and limits:
`/home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/CHECKPOINT.md`.
Validator/rewriting rejection is admission only; CacheExplorer, paused precision
work, AWS and full WP12/WP13 completion remain excluded.


## Accepted ten-case AWS comparison checkpoint (separate from per-alias qualification) — 2026-10-22

**20/20 gateway expectation passes; 10/10 pairs conform to approved comparison contracts; 0/10 byte-identical response pairs; Backend/JWKS execution counts unmeasured.**

This checkpoint does not upgrade any per-alias qualification row above. It closes the named ten-case direct-Invoke comparison scope only: four pet-store routes, malformed-JSON/missing-token/invalid-token rejection and their subsequent valid recovery cases. The accepted [AWS evidence](/home/steve/workspace/artifacts/implementation/light-lambda/WP17-java-netty-aws-20261022T210550Z/REPORT.md) supplies the gateway results; the existing [offline field disposition](/home/steve/workspace/artifacts/implementation/light-lambda/WP17-offline-parity-review-20261022T211639Z/REPORT.md) accounts for all 30 differing fields through the approved envelope, header-casing, statusDescription and error-response-policy contracts. Contract conformance is distinct from byte identity: the unchanged raw comparator still reports zero exact pairs and ten differences (exit 1). No normalization, expectation changes or blanket waiver is implied.

This documentation-only closure uses existing evidence. No reruns, tests, builds, AWS calls, deployments, deletions or commits were performed for it. Original results, failures, ZIPs and evidence remain preserved. Java HTTP500/FunctionError comparison, Validator, Router rewriting, general header multiplicity, public ingress, paused experiments, observability promotion, full WP12/WP13 and all-handler parity remain outside this closure. Backend/JWKS counts cannot be inferred from gateway START or Invoke API receipts. AWS cleanup remains a separate explicit decision; all nine resources remain retained and no deletion is authorized.


### Separate proxy transport integration — 2026-10-22

Fourteen local pairs/44 requests exercise unchanged packaged Java/Rust gateways through real signed SDK Invoke HTTP to running A/B backends (42 distinct execution entries/completions). This is an additional proxy-only host/owned-Runtime-API profile, not a new RIE run or handler-row qualification upgrade. Injected-client, RIE/package and accepted ten-case AWS evidence remain separate. [Report](/home/steve/workspace/artifacts/implementation/light-lambda/WP17-parity-integration-20261022T224001Z/REPORT.md). Stage3 scope is prepared, not deployed; AWS cleanup requires a separate decision.

## 2026-10-27 explicit in-process candidate: composed RS256 qualification

[Report and case-level observations](/home/steve/workspace/artifacts/implementation/light-lambda/validator-jwt-composition-20261027T223211Z/REPORT.md).
Separate package SHA-256 `d460bc21b7a8a69130991c206301b6da2d05de1588cdccd9c574266a136132bb`;
accepted baseline/default package, alias counts and earlier rows remain unchanged.
22 corrected batch requests count once; do not multiply by the four participants.

| Participant | New combined evidence | Boundary |
|---|---|---|
| specification | Captured pet-store source/operation scopes; real Init before immutable publication | Sample requires POST pets.uad; source unchanged |
| jwt | RS256/JWKS verification, missing/malformed/signature/issuer/audience/expiry/scope rejection; alternating verified identities | 1 measured public-key fetch/reuse; HTTP synthetic JWKS, not TLS/refresh/recovery; UnifiedSecurity alias not newly tested |
| validator | Valid JWT + schema/parser/header rejection before dispatch, warm recovery | Explicit in-process binary; ordinary default remains closed |
| proxy | 11 exact body/method/path/identity observations and unique downstream request IDs | Owned Invoke endpoint, no AWS/SDK-wire proof; backend errors not covered by this new composition |

Final22/22 response expectations pass;11 zero-dispatch rejects,11 Invoke calls,
1 JWKS GET and1 separate missing-token Init probe401/0/0. The initial attempt's
scope-fixture failure is retained (3 requests,0 Invoke,1 JWKS); no expectation or
spec was normalized. New local-only configuration layer SHA-256
`0f7af9f33d3558f12834df9f63c6d7343ac9a29e8e5dcbc3b87fb16df4bb84de`.
Accepted artifacts, original fixture failures, Traceability instrumented-only proof,
Router rewriting, paused experiments, AWS and full WP12/WP13 remain separate/open.

## 2026-10-27 repaired in-process candidate: expanded package regression

[Full report and per-profile counts](/home/steve/workspace/artifacts/implementation/light-lambda/validator-composed-errors-20261027T224019Z/REPORT.md).
New local candidate ZIP SHA-256
`ef530db8ca84a939faa8bf9e0da6a35f7c3d8747b0527e7fbb07a41adddd251c`.
Accepted baseline and previous candidate artifacts/rows remain historical.

| Participant / scope | Additional actual package evidence | Qualification limit |
|---|---|---|
| Specification | Restored metadata-only offline/Init when Validator disabled; original7-request and composed26-request profiles pass unchanged | Initial preflight/real-Init failures preserved; typed selector repair, no fixture edits or forced Validator activation |
| Validator + JWT + proxy | Prior21/22 plus backend95 and JWKS22 profiles retested on repaired package | Explicit entry only; ordinary default activation closed |
| Proxy response/error handling |42 bounded modes,95 requests/90 Invoke completions: business500 vs FunctionError, invalid envelope, API errors, headers/base64/body variants and recovery | Owned transport proof, not AWS IAM/platform-timeout/raw SDK-wire measurement |
| JWT/JWKS |22 fresh-token RS256 cases,16 Invokes/11 measured JWKS GETs, actual recovery after503/malformed/empty and unknown/same-kid rotation | Local refresh0 profile only; production30s cooldown and TLS/concurrency claims separate; no principal-cache inference |
| Existing supported aliases / partial X-04 | All applicable original handler families, composed chain, LoggerSet and selected baseline pass in19 total jobs |310 main request assertions count once; admission negatives are not handler execution; Traceability state remains instrumented-only |

Overall received-event reconciliation:187 local Invoke requests with187 unique
forwarded gateway IDs and20 JWKS GETs across profiles.317 actual RIE request starts
include310 main checks,5 Init probes and2 extra LoggerSet startup checks. Baseline's
original unavailable-Validator assertion applies to ordinary bootstrap, not this
explicit entry; retained unchanged and excluded by a predeclared selection only.

One Specification-only integration defect repaired; no unresolved contract
change is needed for these passes. Strict Clippy remains non-green in unchanged
shared code; full WP12/WP13, AWS, default activation/resources, Router rewriting,
original fixture/Java exchange findings and paused experiments remain open.
