# Packaged handler profiles (local L2)

These opt-in profiles reuse the bootstrap-only ZIP without changing production
handlers. Each creates real configuration, validates it through the packaged
application's offline Init, builds a configuration layer, and mounts it read-only
in pinned AL2023 RIE. Requests use actual Init/factory/service/SDK paths. Only
owned business Invoke, JWKS and HTTP dependencies are fake.

From `/home/steve/workspace/light-fabric-light-lambda`, choose a fresh output root:

```bash
L2_PACKAGE=/home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/reproducible-package-v2/bootstrap.zip
L2_OUT=/home/steve/workspace/artifacts/implementation/light-lambda/my-l2-rerun
rtk proxy python3 -B apps/light-lambda/tests/l2/profiles.py --package "$L2_PACKAGE" --profile headers --out "$L2_OUT/headers"
rtk proxy python3 -B apps/light-lambda/tests/l2/profiles.py --package "$L2_PACKAGE" --profile jwt --out "$L2_OUT/jwt"
rtk proxy python3 -B apps/light-lambda/tests/l2/profiles.py --package "$L2_PACKAGE" --profile router --out "$L2_OUT/router"
rtk proxy python3 -B apps/light-lambda/tests/l2/profiles.py --package "$L2_PACKAGE" --profile transformers --out "$L2_OUT/transformers"
```

| Profile | Enabled handlers | Requests | Assertions |
|---|---|---:|---|
| headers | correlation, requestHeader, proxy, responseHeader | 5 | First correlation scalar/repeated multimap; unrelated last scalar; fresh generated IDs; global/prefix mutations; backend error; warm isolation |
| jwt | jwt→proxy and separately security→proxy | 8 each | A/B/A principals; missing/forged/issuer/expiry/audience rejection; three Invokes and one JWKS fetch per chain; no token logs |
| router | router→router→proxy | 9 | HTTP GET/POST body rules; service_id removed; Lambda fallback; unknown-service/HEAD/transport ERR10089; no redirect following/double dispatch; warm recovery |
| transformers | requestTransformer→proxy→responseTransformer | 6 | Preserved native fixture's Rust CEL pair transforms both bodies; request/response overflow ERR14032; zero dispatch on input rejection; warm recovery; nonmatching path untouched |

All 36 family requests execute the service, including expected service errors.
These are not admission-only tests. Each chain observes one immutable snapshot,
one cold request, subsequent warm requests, unique SDK request IDs and exactly
one audit finalization per request. Paired Transformer input rejection retains
the approved ERR14032 response-context behavior. Request-only ERR14031 remains
separate built-app L1 coverage, not an L2 claim.

JWT credentials/claims are synthetic. Dependency records allowlist headers and
synthetic subject/principal IDs, excluding bearer and SDK signing values. JWKS
uses owned numeric-loopback HTTP: TLS JWKS remains unqualified. Containers share
only their owned network namespace; no AWS/shared service or host networking is
used. Context-manager cleanup removes owned containers on success/failure.

Outputs include real source configuration, layer ZIP/report, package identity,
results.json, redacted dependency observations, RIE logs and container inspect
records. Preserve failed outputs; reruns require fresh directories.

The package SHA-256 remains
`24955f85bcc632989f6c8ed772c611dbab5530b8813abc14026beb64a6a47bc4`.
Build commands and earlier two-build reproducibility evidence are in
[PACKAGING.md](../../PACKAGING.md). This iteration verifies and reuses those
bytes, without claiming a new reproducible build.

Package/RIE-qualified aliases: **proxy, correlation, requestHeader,
responseHeader, jwt, security, router (forwarding only), requestTransformer,
responseTransformer**. This covers listed configurations, not every option.

The subsequent [additional profiles](REMAINING-PROFILES.md) qualify13 of the
remaining14 admitted aliases; Traceability remains blocked on observable state
proof. See the [per-alias matrix](L2-MIGRATION-MATRIX.md) for current package
status versus separate component/built-app/comparison evidence.

Validator and nonempty Router rewrite rules still fail Init. Baseline negative
RIE and packaging tests prove admission only. Precision-child execution/platform
clearance, paused experiments, v6 reproducibility, Router rewriting, full
WP12/WP13 and AWS qualification remain open.

Focused transport regressions:

```bash
rtk proxy python3 -B -m unittest discover -s apps/light-lambda/tests/l2 -p test_profiles.py -v
```
