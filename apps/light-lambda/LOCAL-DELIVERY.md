# Historical local milestones 1 and 2 profile

**Superseded status and commands:** use [REGISTERED-DELIVERY.md](REGISTERED-DELIVERY.md).
The results below describe the initial delivery. Repeated-correlation is now
fixed, the original profile contains eight fixtures, and Router forwarding is
registered. Historical failure evidence remains unchanged.

Run from `/home/steve/workspace/light-fabric-light-lambda`.

Build the actual Lambda entry point:

```sh
rtk cargo build --locked -p light-lambda --bin bootstrap
```

One reproducible application invocation command:

```sh
rtk cargo test --locked -p light-lambda --test l1 local_baseline -- --nocapture
```

The checked-in layer is [examples/local-baseline](examples/local-baseline).
**Enabled chain: Lambda proxy only.** JWT/unified security are explicitly disabled;
other policies retain disabled embedded defaults. This is a local test profile.
The L1 harness copies that layer, injects its owned fake Invoke endpoint, starts
the built `bootstrap` with a cleared environment and synthetic credentials, and
sends two ALB events through its loopback Runtime API. It asserts the forwarded
business event, success `200 / hello from business`, response header, and business
FunctionError `502 / ERR14014`. The second test enables Router and Validator
separately and asserts natural exit 1, one explicit `/init/error`, no polling,
and no Invoke/JWKS calls. No Router/Validator worker is started. The harness
terminates the successful warm runtime after assertions; it does not claim a
natural successful process exit. Set `WP10_L1_EVIDENCE_DIR` to a fresh directory
to retain the existing harness's sanitized wire/log evidence.

## Incremental native comparisons

Use a fresh output path; existing output directories/files are rejected:

```sh
JAVA_RESULTS=/home/steve/workspace/artifacts/implementation/light-lambda/WP3/final-qualification-20261011T181646.520429Z/whole-java-corpus/results
rtk bash apps/light-lambda/scripts/compare-local.sh \
  "/tmp/light-lambda-compare-$(date -u +%Y%m%dT%H%M%SZ)" "$JAVA_RESULTS"
```

This script builds `bootstrap`, then runs and compares seven original native
fixtures: four `E-03-response-off-{0,1,2,3}-multi-v1` proxy body cases,
`Q-02-empty-correlation-multi-contract-v1`,
`B-04-Q-02-header-prefix-correlation-multi-contract-v1`, and
`B-04-remove-update-prefix-collision-multi-contract-v1`.
The registered subset is proxy, correlation, traceability, requestHeader and
responseHeader. Passing these fixtures qualifies only their exercised behavior.
All seven are intentional differences; each runner is asserted against its own
approved expectations and the observed difference set must exactly equal the
declared set. Missing/stale differences fail. Backend targets/counts cannot be
excluded. Response header names are case insensitive, values and within-name
order exact; bodies are exact. Forwarded events use the declared subset plus
exact-map and absent-field assertions. Absence, null and empty remain distinct.
No nondeterministic masking is applied by these adapters.

The Java directory contains **older WP3 Java executions**, not fresh Java runs.
The comparator revalidates each selected result's schema, runner and fixture
digest and rejects missing/duplicate results. Use another Java result directory
with matching digests for a new Java execution. Original fixtures are unchanged.

Individual fixture selection and comparison:

```sh
rtk cargo run --locked -p serverless-conformance -- run \
  --fixture Q-02-empty-correlation-multi-contract-v1 --out /tmp/ll-rust-one
rtk cargo run --locked -p serverless-conformance -- compare \
  --fixture Q-02-empty-correlation-multi-contract-v1 \
  --java "$JAVA_RESULTS" --rust /tmp/ll-rust-one --out /tmp/ll-compare-one
```

Repeat `--fixture` and/or `--handler` to take their union. Handler selection uses
the native class inventory's aliases and selects fixtures declaring that class,
including disabled configurations; it does not infer policy coverage from a
ledger link. With no selection flags, the entire inventory is selected and the
current incomplete implementation exits non-zero.

```sh
# Expected exit 1: all 23 Router fixtures lack an admitted runner adapter.
rtk cargo run --locked -p serverless-conformance -- run \
  --handler router --out /tmp/ll-router-incomplete
# Expected exit 1: a real expectation mismatch, not unimplemented or skipped.
rtk cargo run --locked -p serverless-conformance -- run \
  --fixture Q-02-repeated-correlation-multi-contract-v1 --out /tmp/ll-repeated-failure
```

Each command writes `coverage.json` separately from result-v1: selected/total,
pass, fail, selected unimplemented, unselected, complete unsupported-adapter list,
per-native-class counts and one row per fixture with its kind. A selected missing
adapter is incomplete and exits 1; it produces no fabricated result or pass.
`skipped` remains reserved for result-v1's existing single-runner contract.
Single-runner, generated-correlation, key/token/JWKS, warm-step and specialized
probe adapters are currently unimplemented. The initial adapter set is explicit
in `crates/serverless-conformance/src/incremental.rs`.

Execution writes each fixture's configuration into a fresh temporary layer and
uses the app's real factory registrations, real Init and guarded service. The
fake Backend replays `backend[]`, uses the production REST-v1 event builder and
response parser, and fixes its event timestamp. Service deadlines retain the
existing relative-wall model. No SDK/provider or worker is constructed for this
in-process profile. The separate baseline exercises actual SDK Invoke over HTTP.

The repeated-correlation fixture expects `test-only-first` in the forwarded
scalar header; the existing REST-v1 encoder emits `test-only-second`, preserving
both original values in the multimap. That mismatch is retained as a failing
result. This milestone changes neither the native expectation nor the settled
REST-v1 scalar behavior.

## Verification and remaining gates

```sh
rtk cargo test --locked -p light-lambda -p serverless-conformance
rtk cargo check --locked --workspace --all-targets
rtk cargo clippy --locked -p light-lambda -p serverless-conformance --all-targets --no-deps -- -D warnings
rtk cargo fmt -p light-lambda -p serverless-conformance -- --check
```

Final evidence is under
`artifacts/implementation/light-lambda/milestones-1-2-20261018T034957Z/` in the
workspace, with unique command logs, exit records, sanitized built-app evidence
and separate run/comparison coverage. The package suite records 165 passes,
including 45 built-app L1 cases; the final comparator refinement has additional
focused verification. Workspace compile and changed-package lint/format pass.
Workspace-wide formatting reports inherited
`frameworks/light-lambda-runtime/src/router_java_worker/recovery.rs`; that paused
work remains untouched. A full workspace test run was not attempted because
these milestones do not resume the paused worker experiments.

Milestones 1 and 2 are local subset deliveries. **WP12 remains incomplete; WP13
is started and incomplete.** Of 480 fixtures, eight have adapters, seven pass and
one fails. The seven-case profile leaves 473 unselected; 472 inventory fixtures
lack adapters. A full-inventory admission run selects480, reports7 pass/1 fail/
472 unimplemented/0 unselected, and exits1 without starting unsupported workers.
Router/Validator integration, full per-kind adapter coverage,
full-inventory compare, broader final work-package gates, packaging and AWS
qualification remain open. No deployment or AWS call is part of these commands.
