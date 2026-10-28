# Additional admitted-handler L2 profiles

Use the unchanged bootstrap-only ZIP, pinned RIE/fake images and owned numeric
loopback dependencies. No AWS endpoint, precision process or shared service is
used. Each configuration layer is validated by the actual packaged offline
Init, then mounted read-only for real Init/factory/service/SDK requests.

From `/home/steve/workspace/light-fabric-light-lambda`, choose a fresh output root:

```bash
L2_PACKAGE=/home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/reproducible-package-v2/bootstrap.zip
L2_OUT=/home/steve/workspace/artifacts/implementation/light-lambda/my-additional-l2-rerun
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile cors --out "$L2_OUT/cors"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile factors --out "$L2_OUT/factors"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile limit --out "$L2_OUT/limit"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile telemetry --out "$L2_OUT/telemetry"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile token --out "$L2_OUT/token"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile specification --out "$L2_OUT/specification"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile sanitizer --out "$L2_OUT/sanitizer"
rtk proxy python3 -B apps/light-lambda/tests/l2/remaining_profiles.py --package "$L2_PACKAGE" --profile admin --out "$L2_OUT/admin"
```

| Profile | Newly qualified aliases | Service requests | Observable behavior/error/isolation |
|---|---|---:|---|
| cors | requestCors, responseCors | 6 | Allowed-origin echo; preflight headers/null response with one native-contract OPTIONS dispatch; denied origin403 ERR10092/zero dispatch; origin-free warm response has no stale echo; backend502; successful warm response |
| factors | basic, apikey (separate chains) | 5 each | Valid/missing/wrong credential; Basic path denial ERR10071; invalid credentials ERR10047/ERR10075; warm success after rejection; no forged principal; API-key unprotected path bypass |
| limit | limit | 5 | Two configured address quotas independent;200→429 ERR10088; exhausted buckets remain exhausted; exact limit/remaining and bounded positive reset; no success headers because headersAlwaysSet=true is unavailable |
| telemetry | metrics, audit | 5 | Closed EMF Route template/unmatched/error counts; no raw-path dimensions; audit posted status, trusted service ID, masking, no selected-header carryover; success/error/recovery; exactly one policy record each per request |
| token | token | 6 | Typed provider denial401, malformed provider500, recovery, client-credentials request, cache reuse; primary/secondary token placement verified only as booleans; unmatched prefix has no stale auth; four business calls/three provider fetches |
| specification | specification | 7 | Real local OpenAPI scopes; write403/read200/write403; invalid JWT401; unmatched404/method405; source file changed in mounted layer after Init yet captured specification continues working; warm read200 |
| sanitizer | sanitizer | 4 | Base64 JSON HTML encoding; malformed JSON400 ERR10015/zero dispatch; plain body unaffected; repeated encoded request matches without contamination |
| admin | health, info, getLogger | 10 | Exact health/redacted module/current-filter responses; protected-chain401 without token; reserved POST404/zero dependency dispatch; authenticated→unauthenticated→authenticated isolation; cached JWKS |

These53 new service requests are9 chains (9 cold/44 warm), with25 signed
business Invoke calls,3 OAuth fetches and2 JWKS fetches. Every request has one
framework audit finalization and each chain one immutable snapshot. All error
cases are real service executions; none is an Init-admission substitute.
The original36 family requests are rerun separately and remain green:89 total
family requests, not including the proxy-only baseline/admission cases.

The Specification test temporarily changes only its owned extracted OpenAPI
file from the host after Init, then restores the exact bytes before manifest
verification. The container mount remains read-only; original fixtures and
source configuration are preserved. This proves use of captured resources.

Dependency observations exclude credentials and token values. OAuth requests
record only client-auth presence and exact grant type; business observations
record token-role booleans. Synthetic values appear only in test configuration
and fake reply construction. No real credentials are used. The expanded fake
is an owned test-container script, never a package entry or production helper.

## Traceability blocker

Traceability is selected in the telemetry chain but **not package-qualified**.
`TraceabilityHandler::on_request` writes `ExchangeState.traceability_id`; the
production dispatch adapter puts it in a tracing span. The unchanged package's
formatter explicitly suppresses current-span and span-list fields. The only
wire observation is the original header, which would also pass through without
this handler. Counting it as handler qualification would be false proof.

Existing component/comparison proof of the state and span bridge remains
separate. Further package proof needs an observable consumer of that production
state or a separately reviewed bounded/redacted diagnostic path and a newly
identified package. No such production change or package rebuild is included.

Full per-alias status: [L2-MIGRATION-MATRIX.md](L2-MIGRATION-MATRIX.md).
This is restricted-profile local qualification, not full handler parity.
Validator, Router rewriting, paused precision work and AWS execution remain
excluded; default activation/platform clearance/v6 reproducibility stay open.

Focused fake-transport privacy regressions (4 tests, including the original2):

```bash
rtk proxy python3 -B -m unittest discover -s apps/light-lambda/tests/l2 -p 'test_*.py' -v
```
