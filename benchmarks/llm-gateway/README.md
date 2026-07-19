# LLM Gateway Phase 0 Harness

This directory implements LF-1 benchmark foundations and stores LF-2 evidence.
Inputs, profiles, comparator revisions, schemas, and feature-equivalence claims
are checked in. Raw benchmark reports are retained under `reports/` by the
benchmark operator and must validate against `schemas/result.schema.json`.

## Deterministic mock provider

Run locally:

```bash
cargo run --release -p llm-provider-mock --bin llm-provider-mock
```

or:

```bash
docker compose -f benchmarks/llm-gateway/compose.yml up --build mock-provider
```

The provider exposes `POST /v1/chat/completions`, `/health`, `/metrics`, and
`/metrics.json`. Select a checked-in behavior with `MOCK_PROFILE` or the
`x-mock-profile` request header. Profiles cover stable/variable latency,
overload, slow provider/client, deterministic JSON/SSE usage, and connection
reset behavior. The compatibility corpus includes pinned buffered JSON and SSE
response fixtures as well as the request payloads.

## Baselines

With the mock listening on port 18080:

```bash
./benchmarks/llm-gateway/scripts/run-direct-baselines.sh
```

The direct runner records five 500-RPS, 5,000-RPS, and overload runs. The
fixed-rate load generator reports bounded in-flight saturation, and the closure
validator rejects any saturated result. Run a pinned comparator after building
exactly the descriptor revision:

```bash
./benchmarks/llm-gateway/scripts/run-candidate-baseline.sh bifrost http://127.0.0.1:8081/v1/chat/completions
```

Use identical CPU/memory/TLS settings and the feature-equivalence matrix for
every candidate. The candidate runner validates that matrix before it sends a
request and refuses undeclared feature differences. Five runs are mandatory
for each declared workload.

## PERF-1 and LF-6B architecture checkpoint

The PERF-1 matrix is pinned in `manifests/perf1-manifest.json`. Usage admission
must remain enabled, early audit remains explicitly disabled, and comparator
asymmetries are reported instead of weakening the Light safety floor. While
external reports are being collected, validate the implementation with:

```bash
./scripts/run-llm-architecture-checkpoint.sh --implementation
```

Capture each pinned candidate into the exact closure layout with:

```bash
./benchmarks/llm-gateway/scripts/run-perf1-candidate.sh light http://127.0.0.1:8082/v1/chat/completions
LLM_BENCH_RPS=5000 ./benchmarks/llm-gateway/scripts/run-perf1-candidate.sh light http://127.0.0.1:8082/v1/chat/completions
```

Default closure mode fails until all five-run 500-RPS reports and the short
Light 5,000-RPS sample satisfy admission, absolute latency, and Bifrost
non-inferiority checks. It also requires one `environment.json` per candidate
proving the 2-vCPU/4-GiB setup and CPU/RSS/allocation/task/connection/queue
capture, plus Light's dynamic-versus-sealed `dispatch-allocation.json` at 500
and 5,000 RPS. Missing sidecars fail closed rather than silently weakening the
checkpoint.

```bash
./scripts/run-llm-architecture-checkpoint.sh
```

LF-6B is deliberately thin: one eligible deployment and one attempt, a bounded
channel, one downstream write deadline, disconnect cancellation and permit
recovery, `[DONE]` only on success, and a durable-start barrier before headers.
Multi-provider conversion and the full slow-client matrix remain deferred.

## LF-8, PERF-2, and LF-9

LF-8 replaces the process-only audit seam in production projection mode with a
bounded, segmented, checksummed WAL. `local_durable` waits for each
`attempt_started` record to cross the fdatasync watermark before provider
dispatch. A separately credentialed PostgreSQL sink replays bounded batches in
one transaction, treats duplicate event IDs as success, persists its local
checkpoint before reclaiming only fully acknowledged inactive segments, and
never stores prompt/completion/tool/credential content. An OS advisory lock is
held by the writer thread for its entire lifetime, so a duplicate process
cannot concurrently own the same WAL directory even with the same instance ID.
`persistentVolume: true` is an operator attestation, not filesystem detection.
Before using `local_durable` on network storage, verify that the exact mount and
failover configuration preserves `flock`, `fdatasync`, atomic rename, and
post-crash directory-entry durability. An unverified NFS/network mount does not
satisfy the local-durable profile merely because its data outlives a pod.

Set `auditRuntime.sinkDatabaseUrlEnv` to the name of an environment variable
containing the dedicated audit database URL. Apply
`crates/llm-gateway/migrations/audit-postgres/0001_llm_audit.sql` with the
schema-owner role; gateway, auditor, retention, and dataset export privileges
are separate NOLOGIN group roles. Validate with a disposable database:

```bash
./scripts/run-llm-audit-wal-gate.sh \
  postgresql://audit_owner:password@127.0.0.1:5432/llm_audit
./scripts/run-llm-audit-streaming-gates.sh
```

LF-9 supports fallback only before visible output, both provider formats,
bounded per-stream channels, absolute/idle/write-progress/minimum-drain limits,
disconnect cancellation, sanitized terminal error frames without `[DONE]`, and
client-controlled usage visibility while retaining upstream usage for
accounting.

PERF-2 is pinned in `manifests/perf2-manifest.json`. Capture the matched
bounded-async Light/Bifrost runs and the separate Light-only local-durable lane
with `scripts/run-perf2-candidate.sh`. Implementation gates do not manufacture
or substitute performance evidence: Phase 7 remains performance-pending until
the five-run reports and declared CPU/RSS/WAL/fdatasync/commit-wait/sink-lag
sidecars are collected in the named 2-vCPU/4-GiB environment.

## PERF-3, OBS-1, and SEC-1

The release matrix is pinned in `manifests/perf3-manifest.json`. It covers the
500 and 5,000 RPS buffered profiles, full streaming, overload, projection
churn, chaos, and a separately reported local-durable lane. Capture a candidate
with:

```bash
LLM_BENCH_ENVIRONMENT_FILE=/path/to/environment.json \
LLM_BENCH_METRICS_DIR=/path/to/run-sidecars \
  benchmarks/llm-gateway/scripts/run-perf3-candidate.sh \
  buffered-5000 light https://gateway.example/v1/chat/completions
```

The generator must run in a separate constrained process/host and every
candidate/profile requires five runs. The run sidecar records CPU, RSS,
allocation, connection, queue, memory-growth, admission, and recovery evidence.
The implementation gate is intentionally distinct from release closure:

```bash
./scripts/run-llm-release-qualification-gates.sh
./scripts/run-llm-release-qualification-gates.sh --require-performance
```

The first command validates the PERF-3 harness plus the owned OBS-1 and SEC-1
artifact contracts. The second fails closed until all external reports exist
and satisfy the absolute gate. It never creates placeholder measurements.

OBS-1 artifacts live under `operations/llm-gateway`: a metric/label/cardinality
contract, dashboard definitions, alerts, synthetic triggers, a sanitized canary
query, and owned runbooks. SEC-1 artifacts live under `security/llm-gateway`.
The provider boundary disables redirects and ambient proxies, rejects unsafe
URL forms and non-public IP/DNS answers outside development fixtures, and
allowlists non-credential outbound headers. The Pingora LLM handler also
requires proof that body-aware access control ran over the captured bytes before
the preauthorized parser adapter is called.

`manifests/release-manifest.json` keeps `canaryAllowed` false while PERF-3 is
pending. REL-1 must update it only after real performance evidence, synthetic
alert exercises, security approval, and artifact digests are recorded.

## LF-2 evidence and gates

Generate machine-local measurements explicitly:

```bash
./benchmarks/llm-gateway/scripts/generate-evidence.sh
```

Validate implementation artifacts without claiming external benchmark closure:

```bash
./scripts/run-llm-phase0-gates.sh --implementation
```

The default/`--closure` gate additionally requires all external baseline lanes
to be complete and therefore fails closed while a manifest lane is
`pending-external`. Do not relabel a lane `pass` until its raw results and
environment manifest are checked in or attached to the release evidence.
