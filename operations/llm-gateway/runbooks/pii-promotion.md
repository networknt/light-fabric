# PII profile promotion

PII promotion is separate from the metadata-only MVP release. Never infer
durability or performance approval from passing functional/security tests.
Every evidence file must identify the exact model, detector version, token
format, scope, and vault implementation version being promoted.

Request scope keeps reversible mappings only in request memory. It must create
no vault client, task, network call, or vault allocation. `leave_masked` is the
only streaming policy. `reject_buffered` requires a fully buffered response and
must reject altered or unresolved placeholders without returning raw content.

Session and host scopes remain unavailable until the durable lane passes on a
dedicated regional vault. The Portal database, audit database, SQLite, and
replica-local caches are forbidden vaults. PostgreSQL vault deployment uses
independent credentials/TLS, externally referenced encryption keys, enforced
TTL, access audit, backup/recovery, failover, and deletion drills. The gateway
has exact-key operations and no scan/export privilege.

Run each lane independently:

```bash
./scripts/run-llm-pii-promotion-gates.sh --functional
./scripts/run-llm-pii-promotion-gates.sh --security
./scripts/run-llm-pii-promotion-gates.sh --durability
./scripts/run-llm-pii-promotion-gates.sh --performance
./scripts/run-llm-pii-promotion-gates.sh --promote
```

Capture each PERF-4 profile from a separate load-generator process with a
profile-specific configured target and payload:

```bash
LLM_BENCH_ENVIRONMENT_FILE=/evidence/environment.json \
LLM_BENCH_METRICS_DIR=/evidence/request-memory-buffered \
  ./benchmarks/llm-gateway/scripts/run-perf4-profile.sh \
  request-memory-buffered https://gateway.example/v1/chat/completions /evidence/pii-request.json
```

Repeat for all four manifest profiles. Preserve the raw fixed-load, capacity
sweep, and sidecar files, then aggregate them into
`benchmarks/llm-gateway/reports/perf4/summary.json`. The validator requires the
five 500-RPS runs, an open-loop capacity sweep, every sidecar measurement, and
zero vault clients, tasks, calls, and allocations for both request-memory
profiles.

The default command validates implementation contracts only. Closure commands
fail until their own real evidence exists. `--promote` passes only when every
lane passes for one identical promotion identity; evidence from different
models, detectors, formats, scopes, or vault versions cannot be combined.
