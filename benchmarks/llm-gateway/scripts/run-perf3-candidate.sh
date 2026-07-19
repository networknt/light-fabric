#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <profile> <light|bifrost|agentgateway> <target-url>" >&2
  exit 2
fi

profile="$1"
candidate="$2"
target="$3"
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
manifest="$repo_dir/benchmarks/llm-gateway/manifests/perf3-manifest.json"
environment_file="${LLM_BENCH_ENVIRONMENT_FILE:-}"
metrics_dir="${LLM_BENCH_METRICS_DIR:-}"

if [[ -z "$environment_file" || -z "$metrics_dir" ]]; then
  echo "PERF-3 requires LLM_BENCH_ENVIRONMENT_FILE and LLM_BENCH_METRICS_DIR" >&2
  exit 2
fi
jq -e --arg profile "$profile" --arg candidate "$candidate" '
  .profiles[$profile] != null and
  (.profiles[$profile].candidates | index($candidate)) != null
' "$manifest" >/dev/null
jq -e --arg profile "$profile" --arg candidate "$candidate" '
  .profile == $profile and .candidate == $candidate and
  .generatorSeparate == true and .tls == true and
  (.revision | test("^[0-9a-f]{40}$")) and
  (.generatorRevision | test("^[0-9a-f]{40}$")) and
  (.providerDigest | test("^[0-9a-f]{64}$"))
' "$environment_file" >/dev/null

if [[ "$candidate" != "light" ]]; then
  cargo run --locked -p llm-phase0-spikes -- validate-comparison "$candidate"
fi

offered_rps="$(jq -r --arg profile "$profile" '.profiles[$profile].offeredRps' "$manifest")"
payload_name="$(jq -r --arg profile "$profile" '.profiles[$profile].payload' "$manifest")"
runs="${LLM_BENCH_RUNS:-5}"
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
report_dir="$repo_dir/benchmarks/llm-gateway/reports/perf3/$profile/$candidate"
mkdir -p "$report_dir"
cp "$environment_file" "$report_dir/environment.json"

for run in $(seq 1 "$runs"); do
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$repo_dir/benchmarks/llm-gateway/payloads/$payload_name" \
  LLM_BENCH_OUTPUT="$report_dir/${offered_rps}rps-run${run}.json" \
  LLM_BENCH_CANDIDATE="$candidate" \
  LLM_BENCH_PROFILE="$profile" \
  LLM_BENCH_RPS="$offered_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen

  metrics_file="$metrics_dir/run${run}.json"
  jq -e --argjson required "$(jq --arg profile "$profile" '[.requiredSidecarFields[], .profiles[$profile].requiredMeasurements[]?] | unique' "$manifest")" '
    . as $metrics | $required | all(. as $name | $metrics[$name] != null)
  ' "$metrics_file" >/dev/null
  cp "$metrics_file" "$report_dir/${offered_rps}rps-run${run}-metrics.json"
done
