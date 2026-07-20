#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <profile> <target-url> <payload-file>" >&2
  exit 2
fi

profile="$1"
target="$2"
payload="$3"
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
manifest="$repo_dir/benchmarks/llm-gateway/manifests/perf4-manifest.json"
environment_file="${LLM_BENCH_ENVIRONMENT_FILE:-}"
metrics_dir="${LLM_BENCH_METRICS_DIR:-}"
runs="${LLM_BENCH_RUNS:-5}"
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
sweep_rps="${LLM_PERF4_SWEEP_RPS:-100 250 500 750 1000}"

if [[ -z "$environment_file" || -z "$metrics_dir" ]]; then
  echo "PERF-4 requires LLM_BENCH_ENVIRONMENT_FILE and LLM_BENCH_METRICS_DIR" >&2
  exit 2
fi
if [[ ! -f "$payload" ]]; then
  echo "PERF-4 payload is not a regular file: $payload" >&2
  exit 2
fi
if [[ "$runs" -ne "$(jq -r '.runsPerProfile' "$manifest")" ]]; then
  echo "PERF-4 requires exactly $(jq -r '.runsPerProfile' "$manifest") fixed-load runs" >&2
  exit 2
fi
jq -e --arg profile "$profile" '.profiles[$profile] != null' "$manifest" >/dev/null
jq -e '
  .generatorSeparate == true and .tls == true and
  (.revision | test("^[0-9a-f]{40}$")) and
  (.generatorRevision | test("^[0-9a-f]{40}$")) and
  (.providerDigest | test("^[0-9a-f]{64}$")) and
  (.identity.model | length > 0) and
  (.identity.detectorVersion | length > 0) and
  (.identity.tokenFormatVersion | length > 0) and
  (.identity.scope | IN("request", "session", "host")) and
  (.identity.vaultImplementationVersion | length > 0)
' "$environment_file" >/dev/null

report_dir="$repo_dir/benchmarks/llm-gateway/reports/perf4/$profile"
mkdir -p "$report_dir/fixed" "$report_dir/sweep"
cp "$environment_file" "$report_dir/environment.json"

capture_run() {
  local offered_rps="$1"
  local output="$2"
  local metrics="$3"
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$payload" \
  LLM_BENCH_OUTPUT="$output" \
  LLM_BENCH_CANDIDATE="light" \
  LLM_BENCH_PROFILE="$profile" \
  LLM_BENCH_RPS="$offered_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
  jq -e --argjson required "$(jq '.requiredMeasurements' "$manifest")" '
    . as $metrics | $required | all(. as $name | $metrics[$name] != null)
  ' "$metrics" >/dev/null
}

for run in $(seq 1 "$runs"); do
  output="$report_dir/fixed/500rps-run${run}.json"
  metrics="$metrics_dir/fixed-run${run}.json"
  capture_run 500 "$output" "$metrics"
  cp "$metrics" "$report_dir/fixed/500rps-run${run}-metrics.json"
done

sweep_run=0
for offered_rps in $sweep_rps; do
  sweep_run=$((sweep_run + 1))
  output="$report_dir/sweep/${offered_rps}rps.json"
  metrics="$metrics_dir/sweep-run${sweep_run}.json"
  capture_run "$offered_rps" "$output" "$metrics"
  cp "$metrics" "$report_dir/sweep/${offered_rps}rps-metrics.json"
done

echo "PERF-4 raw evidence captured for $profile. Aggregate all profiles into reports/perf4/summary.json, then run --performance."
