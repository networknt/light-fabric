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
