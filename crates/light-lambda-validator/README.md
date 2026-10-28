# In-process precision Validator

This library runs JSON Schema request validation in the gateway process. It uses
`jsonschema` 0.48.0 with the existing diagnostic/exact-number and captured-reference
patches. No worker, IPC, executable admission, remote schema retrieval, custom
allocator or process-wide JSON feature change is required.

The existing `precision.rs`, `catalogue.rs`, `request.rs` and `status.rs` are copied
unchanged from the preserved engine source. `SOURCE-PROVENANCE.json` records their
identities and every vendored dependency file. The private packages retain their
original Rust library names but use distinct Cargo package identities:

- `light-lambda-precision-jsonschema`: jsonschema 0.48.0, default features off.
- `light-lambda-precision-referencing`: referencing 0.48.0 with captured entries.
- `light-lambda-precision-json`: serde_json 1.0.149 with arbitrary precision,
  insertion order and the existing body-parser recursion support.

The gateway's canonical serde_json remains a separate package; precision JSON
values cross the library boundary as bytes/strings only. No numeric fallback is
introduced. Compilation eagerly prepares captured schemas before requests.
JSON/YAML source bounds and the original error formatter remain in effect.
A lexical body-depth check precedes the preserved recursive request-body parser.

Use `light_lambda_runtime::validator_ipc::in_process::InProcessEngine` with the
existing production Validator provider. The separately named
`validator-in-process` application binary selects this engine explicitly. The
ordinary bootstrap/default activation and accepted ZIP are unchanged.

## Limits and lifecycle

This design shares the gateway's memory and failure domain. Compilation and
validation run on a Tokio blocking thread, not an OS child. Each engine admits
one compilation job; each session admits one queued/active validation job.
Timeout/cancellation closes the session and discards late results; it cannot
preempt synchronous validation. Work tickets, captured data and reservation guards
remain owned until the actual job finishes. Shutdown drains those jobs; immutable
compiled state is released when its last owner drops. Abort/OOM/stack overflow
cannot be caught or isolated by this design.

The local candidate requests a 64 MiB buffer-accounting budget, body depth 128,
and metadata limits of 2 MiB wire / 32 MiB estimated peak / 16 MiB retained.
These are local qualification values, not production memory or CPU bounds.
Compiler heap, allocator overhead and RSS are not comprehensively accounted.
Infrastructure faults close admission; ordinary schema-invalid requests remain
recoverable on the same warm snapshot. Existing ERR14017/504, ERR14004/500 and
ERR14034/503 categories are retained; the in-process unavailable message is
`VALIDATOR_UNAVAILABLE` / `Validator is unavailable.` rather than claiming a worker.

## Local qualification

From the repository root:

```sh
rtk cargo test -p light-lambda-validator --offline
rtk cargo test -p light-lambda-runtime --lib validator_ipc::in_process --offline
rtk cargo test -p light-lambda --test validator_in_process --offline
rtk cargo test -p light-lambda --test l1 validator_in_process:: --offline
```

The last command uses the existing owned Runtime API, Invoke and JWKS servers
with the actual compiled gateway. It includes the checked-in pet-store schema,
valid / invalid / warm-valid requests and the unchanged 15-case semantic matrix.
It creates no AWS resources and starts no precision worker. Set
`WP10_L1_EVIDENCE_DIR` to a fresh directory to retain sanitized evidence. Set
`VALIDATOR_IN_PROCESS_APPLICATION` to an ordinary-built candidate to test that
exact executable instead of Cargo's test-build binary.

Local execution is distinct from Lambda-compatible packaging, RIE/package
qualification, AWS qualification, default activation and full WP12/WP13 closure.
Historical worker evidence/failures and Router rewriting remain separate.
