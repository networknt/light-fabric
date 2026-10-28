# Opt-in in-process Validator qualification

These local profiles reuse the existing L2 RIE harness and owned dependencies.
They do not start a precision child, use AWS, or change the default bootstrap.
Requires Python 3 with cryptography, Docker, and the images pinned in ../images.json.
Run from the repository root. Output directories must not exist.

Build a separately identified, reproducible opt-in package (Cargo Lambda 1.9.2,
Zig, and the locked dependencies must already be available offline):

```bash
python3 apps/light-lambda/scripts/package_baseline.py build /tmp/lambda-in-process-build --entry validator-in-process --cargo-lambda /path/to/cargo-lambda
```

The builder runs two independent optimized builds, requires identical binaries
and ZIPs, and records source and tool identities. Ordinary builds still select
`bootstrap`. Experimental qualification limits are not production resource bounds.

Pin the intended package explicitly; never infer identity from a filename:

```bash
export LIGHT_LAMBDA_TEST_PACKAGE=/absolute/path/to/bootstrap.zip
export LIGHT_LAMBDA_TEST_PACKAGE_SHA256=REPLACE_WITH_REVIEWED_SHA256
python3 apps/light-lambda/tests/l2/in_process/validator.py --package "$LIGHT_LAMBDA_TEST_PACKAGE" --sha256 "$LIGHT_LAMBDA_TEST_PACKAGE_SHA256" --out /tmp/validator-profile
python3 apps/light-lambda/tests/l2/in_process/jwt.py --out /tmp/validator-jwt-profile
python3 apps/light-lambda/tests/l2/in_process/expanded.py --profile backend --out /tmp/validator-backend-profile
python3 apps/light-lambda/tests/l2/in_process/expanded.py --profile jwks --out /tmp/validator-jwks-profile
```

The profiles contain 21, 22, 95 and 22 main request checks respectively, with
Init probes counted separately. Matrices and expectations retain their recorded
contracts. The JWKS profile selects test-only refresh interval zero; production
interval settings are unchanged. Credentials are synthetic and memory-only.
The fake records HTTP dependency observations, not raw SDK-wire or AWS execution
proof. Requalifying an older package validates driver portability only, not a
new source tree. Preserve prior output and use a fresh directory for every run.

Router rewriting, Traceability production observation, default Validator
activation, paused worker/recovery experiments and AWS qualification are separate.

Run all 19 existing profile jobs with at most two in parallel:

```bash
python3 apps/light-lambda/tests/l2/in_process/all_profiles.py --package "$LIGHT_LAMBDA_TEST_PACKAGE" --sha256 "$LIGHT_LAMBDA_TEST_PACKAGE_SHA256" --out /tmp/in-process-all-profiles
```

This includes the 310 main request checks and separately counted Init/admission
probes from the prior qualification. The baseline selector omits only the
ordinary-bootstrap Validator-unavailable contract, which does not apply to this
explicit Validator entry. That original contract is unchanged in `../run.py`.
All other baseline assertions are retained. Traceability remains instrumented-only.

Vendored-source identity and formatting replay are verified separately:

```bash
python3 crates/light-lambda-validator/verify_provenance.py
python3 crates/light-lambda-validator/verify_provenance.py --upstream-root /path/to/pinned/cargo-registry-sources --upstream-root /path/to/pinned/precision-source-inputs
```

The optional upstream roots are verification inputs only. Building and running
the package uses checked-in vendor files, never those historical directories.
