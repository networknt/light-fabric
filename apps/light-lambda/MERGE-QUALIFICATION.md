# Guarded Light Lambda merge qualification

This change adds the Rust Lambda gateway, shared gateway-policy extraction,
serverless AWS transport, conformance fixtures and an explicit in-process
JSON Schema Validator entry. Ordinary `bootstrap` keeps its existing activation
policy. Selecting `validator-in-process` is a separate build choice.

## Review map

- `crates/gateway-policy` and `frameworks/light-pingora`: policy extraction and
  adapters; shared security/runtime/client changes retain the current master
  authentication fixes.
- `crates/serverless-core`, `crates/serverless-aws` and
  `frameworks/light-lambda-runtime`: admission, factory/lifecycle, routing,
  request state, transport, error and completion behavior.
- `crates/light-lambda-validator`: in-process engine with isolated precision
  JSON dependencies. `SOURCE-PROVENANCE.json` retains upstream hashes and records
  formatting derivations; `verify_provenance.py` verifies/replays them.
- `apps/light-lambda`: application entries, supported configuration/layers,
  packaging and the existing L1/L2 harness.
- `tests/serverless-conformance` and `crates/serverless-conformance`: original
  fixtures, versioned Rust expectations, approved differences, retained failures
  and execution/admission reporting. Large generated reviews and characterization
  corpora are evidence, not new production code.

## Local results

Qualification used base `9267419d3ac9288688c41ac5e99193bbb105d6e3`, Rust 1.99.1,
locked dependencies and owned local dependencies. No AWS operation was performed.

| Check | Result |
|---|---|
| Workspace formatting and all-target compilation | Passed |
| Strict Clippy: all targets of the seven Lambda-related packages | Passed, no new lint waiver |
| Complete workspace tests | 3,765 passed; 0 failed; 242 ignored |
| Explicit isolated PostgreSQL CI lanes | 49 passed, separately counted |
| Python packaging/L2/static-inventory tests | 51 passed |
| Workflow contracts, client templates, image orchestration, namespace preflight | Passed |
| Runner image and Kubernetes rendering/placeholder checks | Passed locally |
| Vendor provenance | 160 files verified; 11 formatting transformations replayed |
| Optimized package | Two independent clean builds are byte-identical |
| Packaged RIE | 19/19 jobs; 310 main checks; 317 request starts including separate probes |
| Local dependency observations | 187 Invoke requests/unique gateway IDs; 20 JWKS GETs |

Source test runs used debug-symbol/incremental-cache reductions to bound disk
use; test selection and assertions were retained. The test-only SHA-256 build
optimization preserves the original self-image admission deadline. No ignored
precision/Router execution gate was enabled. PostgreSQL CI fixtures were owned,
loopback-only, synthetic and removed afterward.

The SDK operation-cap, HTTP client-setup and Gateway fixture repairs preserve
timeouts, response contracts and measured downstream counts. Added priming calls
are separately counted. Empty TCP probes are not HTTP requests, and selected
Gateway test ports cannot be reused by another test in the same process.

The original D-08e concurrent test passes unchanged. Its historical zero-probe
failure remains preserved: per-fixture counters are isolated, but the old record
lacks timing sufficient to reconstruct exactly where the 50 ms budget expired.
No expectation, retry or suite-wide serialization was introduced to hide it.

Workspace-wide Clippy is a separate non-green baseline check, also failing on
pristine master. These findings were not waived or silently fixed through
unrelated API/layout changes. Remote CI must qualify the committed PR head.

## Local package identity and rerun

The bootstrap-only x86-64 AL2023 package has SHA-256:

`2f07f2446c5c50280f724d328ad3d4eb66affb7b19b81101ac885b963afd1308`

It is a local qualification artifact, not an accepted-release replacement.
The accepted ZIP remains
`d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a`.
The build manifest records source/tool identities and both independent builds.
No executable ZIP or private trial credential is committed in this change.

See [portable build and rerun commands](tests/l2/in_process/README.md) and
[delivery handoff](DELIVERY-HANDOFF.md) for artifact locations and supported
profiles. The supported tooling does not require RTK. Production compiler
fingerprints show the conformance-worker hook disabled and canonical
serde_json precision features unchanged; the private precision crate is isolated.

## Open scope

This is an incremental guarded merge, not full WP12/WP13 closure. Default
Validator production activation/resource qualification, Router rewriting and
real-worker provenance/execution, Traceability's instrumented-only proof and
AWS qualification of this package remain separate. Paused precision experiments
remain paused. Original D-08, H-04, Java L-02/D03 conflicts, approved differences
and the broader Java exchange-state defect remain visible. The five existing
Router forwarding comparisons and separate runner counts are retained.

Response/contract parity, byte identity, local HTTP attempts and AWS execution
identity are distinct claims. These local observations do not measure raw
SDK-wire behavior or establish AWS/freeze-thaw/resource bounds.

## Remote CI template repair

The first PR CI run rejected an unregistered Lambda client template. The manifest
now records its existing 30,000 ms early-refresh and 4,000 ms request fallbacks;
the application YAML bytes and qualified package are unchanged. The synchronizer
also resolves the logical light-fabric manifest paths against its own checkout,
so a renamed worktree cannot accidentally check a different sibling clone.
Seven hermetic regression tests cover active/shadow checkouts, drift, missing
and unregistered templates, sibling repositories and write targeting. They pass
on the repair and fail on the old script. CI runs these tests before parity.
The failed remote run remains preserved; this tooling fix requires a new CI run.

The next remote run passed four jobs but the Rust job exhausted disk during
linking, before test assertions. Its logs are retained. The Rust CI job now
uses the locally qualified no-debug-symbol/nonincremental settings and two build
jobs, with a separate cache prefix to avoid restoring large debug-enabled caches.
Debug assertions, test selection, fixture expectations and deadlines are unchanged.
