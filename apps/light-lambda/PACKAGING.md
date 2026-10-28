# Supported baseline package and local RIE

The current consolidated delivery is [DELIVERY-HANDOFF.md](DELIVERY-HANDOFF.md):
exact ZIP/layer identities, supported composition, build/rerun commands,
per-handler qualification and retained open gates. It distinguishes the
recommended default package from the separate Traceability instrumented ZIP.

Profile: `baseline-bootstrap-only-v1`; x86_64, provided.al2023, ZIP contains exactly executable `bootstrap`0755. No worker helpers/JRE/JAR/native helper libraries are installed. [package-profile.json](package-profile.json) is the explicit machine-readable inventory.

Compiled/admitted handlers, within their existing configuration restrictions: **metrics (APM), limit, traceability, correlation, requestCors, responseCors, requestHeader, responseHeader, requestTransformer, responseTransformer, audit, token, router (forwarding only), specification, security, jwt, basic, apikey, sanitizer, proxy, health, info, getLogger**. This is availability, not full parity or RIE coverage for every configuration. Transformers retain their fixed admitted profiles; Info/GetLogger retain redaction/verified-JWT gates. Influx Metrics, SWT, SetLogger and Cache remain deferred/unavailable as recorded in the native inventory. **Enabled Validator and nonempty Router rewrite rules reject Init clearly.** Their source/factory inventory is retained.

The default RIE baseline enables **proxy only**. Separate qualified family profiles enable correlation/headers, JWT/Unified Security, Router forwarding and Transformers; see [profile commands and coverage](tests/l2/PROFILES.md). All use real packaged Init/service paths and owned fake dependencies. Negative Validator/rewrite cases count as admission coverage only. Full WP12/WP13, remaining-handler L2 coverage, TLS JWKS L2-04, precision execution, deployment and AWS qualification remain open.

## Copy-paste build, layer and invocation

Use the existing workspace and the verified cargo-lambda1.9.2 tool from this checkpoint. Zig0.15.2 and the Rust GNU target are installed. These commands use fresh outputs; the build creates two independent clean target directories and removes only its own successful compile caches. Logs/artifacts/provenance remain.

```sh
cd /home/steve/workspace/light-fabric-light-lambda
LL_OUT=$(rtk proxy mktemp -d /tmp/light-lambda-baseline.XXXXXX)
export CARGO_LAMBDA=/home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/tools/cargo-lambda-1.9.2/cargo-lambda
rtk proxy apps/light-lambda/scripts/build-bootstrap.sh "$LL_OUT/package"
rtk proxy apps/light-lambda/scripts/check-zip.sh "$LL_OUT/package/bootstrap.zip"
rtk proxy apps/light-lambda/scripts/build-config-layer.sh apps/light-lambda/examples/local-baseline "$LL_OUT/config-layer.zip" --bootstrap "$LL_OUT/package/bootstrap"
rtk proxy apps/light-lambda/tests/l2/run.sh --package "$LL_OUT/package/bootstrap.zip" --out "$LL_OUT/l2"
```

The last command creates its own endpoint-configured test layer through the same WP14 producer, runs all L2 baseline cases, records results/logs/digests and removes only its owned containers. The separate generic example layer above demonstrates the producer; the RIE layer adds explicit synthetic loopback endpoints. No Config Server or AWS invocation occurs.

For interactive invocation, in terminal A (after building):

```sh
rtk proxy apps/light-lambda/tests/l2/run.sh --package "$LL_OUT/package/bootstrap.zip" --out "$LL_OUT/manual-rie" --serve --port 9000
```

In terminal B:

```sh
cd /home/steve/workspace/light-fabric-light-lambda
rtk proxy curl --fail-with-body -H 'Content-Type: application/json' --data-binary @apps/light-lambda/examples/local-baseline/event.json http://127.0.0.1:9000/2015-03-31/functions/function/invocations
```

The response is ALB200 with `hello from business`; repeat the command for warm invocations. Stop terminal A with Ctrl-C to save logs and remove its two owned containers. The automated mode already asserts one cold success, five subsequent warm successes, representative business error502/ERR14014, malformed ingress400/ERR14012, signed RequestResponse/LogType=None calls, unique request IDs, zero JWKS fetches and exactly seven Invoke calls. It separately requires missing-layer, Validator, Router rewrite and digest-tamper Init rejection with zero dependency calls. `/opt` and `/var/task` are read-only, root is read-only, input hashes stay unchanged, and no EROFS error is observed. Those observations are not a general syscall/no-write proof.

## Toolchain and reproducibility

Cargo-lambda1.9.2 is pinned. This checkpoint used its upstream x86_64-musl release archive, verified against the published SHA-256 (`b5c258c424ebaef39541c3641f33c51137ee6c426016f365628fedb2c737271d`), instead of globally installing it. The standard source-install alternative is `rtk proxy cargo install cargo-lambda --locked --version 1.9.2`; that install path was not executed here. The recorded binary/version/source URL is in the evidence tool manifest. No existing project dependency pin was bumped.

Build uses locked/offline cargo-lambda/Zig, target `x86_64-unknown-linux-gnu.2.34`, `[profile.lambda]` inheriting release with symbol stripping, CARGO_INCREMENTAL=0, SOURCE_DATE_EPOCH from the commit by default, and source/home/target path remapping. The workspace release profile is unchanged. Independent clean builds compare bootstrap and normalized ZIP SHA-256. Only bootstrap is zipped; timestamps/order/mode/extra fields are deterministic. Sidecar manifests identify dirty Rust/Cargo/embedded-config inputs, toolchain, scoped tracked patch and exact commands. This is same-host two-build reproducibility, not a hermetic cross-host supply-chain attestation.

The checker rejects extra entries/helpers, symlinks/traversal, non-0755 bootstrap, non-x86-64 ELF, unexpected DT_NEEDED libraries and GLIBC requirements above2.34. ZIP/native sizes and library/version details are recorded. Generated layer manifests verify exact listed bytes and effective configDigest; Rust validation is authoritative for configuration semantics.

## Local test topology and limits

Images and platform are pinned in `tests/l2/images.json`; actual image IDs/digests are recorded per run. RIE uses the official provided.al2023 image. Fake Invoke/JWKS runs in a separate test-only Python container sharing the owned RIE container's network namespace. This deliberately differs from the plan's host.docker.internal HTTP example: the existing Invoke policy admits insecure test HTTP only on numeric loopback, so the app uses127.0.0.1:19091 without weakening that policy. There is no host networking or product helper in the ZIP. Published RIE/control ports bind host loopback only; fake credentials are literal `test`, and IMDS is disabled.

RIE Init labels/timestamps are emulator observations, not AWS cold-start or memory evidence. L2-04 TLS JWKS remains open; JWT profiles use synthetic loopback HTTP JWKS. The [per-alias matrix](tests/l2/L2-MIGRATION-MATRIX.md) now distinguishes22 restricted-profile RIE-qualified aliases from Traceability's unobservable private-state blocker. Precision-child lifecycle instrumentation is unchanged; actual-child execution still awaits platform clearance. The old v6 helper's reproducibility gap stays open. Bootstrap reproducibility does not close that separate gap.

Current evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z`; exact digests and results are in CHECKPOINT.md, reproducible-package-v2/package-manifest.json and l2-final/results.json.
# Additional handler L2 profiles

Correlation/headers, JWT/Unified Security, Router forwarding and paired
Transformers have opt-in configuration layers and RIE commands in
[tests/l2/PROFILES.md](tests/l2/PROFILES.md), using this same bootstrap-only ZIP.
The baseline default remains proxy-only. Validator and rewriting remain unavailable.

The [remaining-handler profiles](tests/l2/REMAINING-PROFILES.md) add CORS, local
factors, configured limits, telemetry, OAuth tokens, Specification, Sanitizer and
admin terminal checks (53 service requests), retaining the original36 requests.

The [native-order composed profile](tests/l2/COMPOSED.md) tests the supported
business and admin chains together through this same package:26 requests with
ordering, combined errors, real SDK/HTTP dispatch and warm isolation. One rerun
command generates and validates the full configuration layer. Traceability's
instrumented proof stays separate.

