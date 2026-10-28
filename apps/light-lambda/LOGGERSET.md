# Partial X-04: bounded opt-in ROOT diagnostics

The current source has a real LoggerSet provider and atomically reloads the
installed subscriber. It requires both `LIGHT_LAMBDA_ALLOW_LOGGER_SET=1` at
trusted process startup and enabled logging. Gate absence/0 remains closed;
malformed gate values reject Init. The opted-in profile requires an installed
scalar INFO/DEBUG/TRACE startup filter and verified OAuth/JWT authentication.
Compound filters, suppressed INFO levels and invalid filters reject this profile.
Normal gate-closed filter parsing/fallback is unchanged.

Admitted POST commands are `[]` or one object such as
`[{"name":"ROOT","level":"DEBUG"}]`. Only ROOT and INFO/DEBUG/TRACE are
accepted, at most1024 bytes, no unknown fields. Missing/null/empty request bodies
give400 ERR10059. Invalid JSON/schema/name/level/size gives400 ERR11004 with fixed
redacted diagnostics. Validation and authorization precede mutation; the command
does not accept an arbitrary EnvFilter. The reader and reload operation share a
process-owned lock; reload failure publishes no new filter. GET observes that
controller's actual installed filter, with its existing `{"filter":"info"}` shape.

Privacy remains outside the reloaded subscriber. Audit INFO records remain
enabled throughout the admitted level domain; other levels are deliberately
excluded. No logger registry, OFF/ERROR/WARN/ALL, cache inspection or new admin
authentication scheme is implemented. Original native fixture expectations and
the default admission contract remain unchanged.

## Local proof and native reservation qualification

The fixture in `examples/logger-set-qualification/` binds the real provider at
`/qualification/logger`. Existing L1 tooling owns JWT keys and dependencies; it
exercises the actual built debug application, factory, Init and service.

The separately approved native reservation patch is now applied. POST
`/adm/logger` requires the compiled local SetLogger terminal and a preceding
Jwt/Security handler. Runtime reservation checks run before the app callback;
the setter additionally requires an actually verified principal. Native POST
updates followed by truthful GET are built-application-qualified with zero
Invokes. Missing/invalid/skipped authentication, missing authentication chain,
wrong terminal/method and default/proxy mappings preserve the actual filter and
make zero Invokes. Incorrect ordering rejects Init before polling or fetching.
The trusted startup gate and enabled logging remain mandatory.

Current evidence is under
`/home/steve/workspace/artifacts/implementation/light-lambda/WP13-loggerset-native-20261020T081245Z/`.
This remains partial X-04 support, not general LoggerSet or full sample
compatibility. The dependency-boundary defect is now repaired and qualified
without exclusions; see the post-boundary checkpoint below. One initial parallel
conformance EMF timing failure remains recorded separately; its isolated test and
the complete serial conformance suite pass without changed expectations.

Post-boundary evidence:
`/home/steve/workspace/artifacts/implementation/light-lambda/WP13-dependency-boundary-qualified-20261020T123002Z/CHECKPOINT.md`.
The relocated JWKS harness is ignored unless explicitly invoked, and ordinary
app/security builds do not enable its conformance-worker feature. LoggerSet
also passes against the ordinary hook-free debug bootstrap. The delivered ZIP
is unchanged.

Rerun from the worktree with fresh evidence:

```bash
rtk proxy env CARGO_INCREMENTAL=0 WP10_L1_EVIDENCE_DIR=/home/steve/workspace/artifacts/implementation/light-lambda/logger-set-rerun-$(date -u +%Y%m%dT%H%M%SZ) cargo test --locked --offline -p light-lambda --test l1 logger_set:: -- --test-threads=1
```

Actual DEBUG enable/disable, TRACE dependency-value redaction and panic privacy:

```bash
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda-runtime --lib logger_set_tests::actual_reload_privacy_and_panic_redaction
```

The ordinary ignored helper entrypoints are explicitly launched by these parent
tests in isolated application/subscriber processes. They are not precision
workers. Validator acceptance stays ignored pending platform clearance. Paused
experiments and Router rewriting are untouched. No AWS calls or deployment.

The original delivered bootstrap ZIP is unchanged. New debug bootstrap/test executable
identities and exact results are recorded in the checkpoint. No new RIE/package
qualification was claimed at that earlier checkpoint.

## New release and explicit layer tooling — 2026-10-21

The distinct ordinary release ZIP under
`WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip` has now passed
opt-in LoggerSet RIE qualification:24 service requests,2 separate Init rejections,
zero Invokes,2 JWKS fetches across enabled/fresh processes, truthful native
POST/GET, authorization/errors,1024-byte boundary, warm isolation, TRACE
privacy/Audit and fresh INFO reset. Its SHA-256 is
`d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a`.

Ordinary layer tooling continues to strip ambient LIGHT_LAMBDA_* flags. The
explicit layer CLI option `--allow-logger-set` permits offline validation using
the gate; it cannot authorize runtime mutation from a layer. The actual process
must still receive trusted LIGHT_LAMBDA_ALLOW_LOGGER_SET=1 and resolve enabled
logging. An explicit logging.yml.enabled value takes precedence over an unused
logger.enabled substitution value; disable the authoritative field to test a
disabled profile. The default remains closed. Rerun:

```bash
rtk proxy python3 -B apps/light-lambda/tests/l2/logger_set.py --package /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip --out /home/steve/workspace/artifacts/implementation/light-lambda/logger-rie-rerun-$(rtk proxy date -u +%Y%m%dT%H%M%SZ)
```

See that release's CHECKPOINT.md for source/build identities, earlier failed
driver runs, baseline/composed qualification and exact limits. The original ZIP,
fixtures and Java expectations remain preserved. No full X-04, WP12/WP13 or AWS
qualification is claimed.
