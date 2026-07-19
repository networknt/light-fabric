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
