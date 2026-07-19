#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <direct|light|bifrost|agentgateway> <target-url>" >&2
  exit 2
fi

candidate="$1"
target="$2"
if [[ ! "$candidate" =~ ^(direct|light|bifrost|agentgateway)$ ]]; then
  echo "unsupported PERF-1 candidate: $candidate" >&2
  exit 2
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
if [[ "$candidate" != "direct" ]]; then
  cargo run --locked -p llm-phase0-spikes -- validate-comparison "$candidate"
fi

offered_rps="${LLM_BENCH_RPS:-500}"
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
runs="${LLM_BENCH_RUNS:-5}"
if [[ "$offered_rps" == "5000" && "$candidate" == "light" && -z "${LLM_BENCH_RUNS+x}" ]]; then
  runs=1
fi
payload="$repo_dir/benchmarks/llm-gateway/payloads/small-text.json"
report_dir="$repo_dir/benchmarks/llm-gateway/reports/perf1/$candidate"
mkdir -p "$report_dir"

for run in $(seq 1 "$runs"); do
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$payload" \
  LLM_BENCH_OUTPUT="$report_dir/${offered_rps}rps-run${run}.json" \
  LLM_BENCH_CANDIDATE="$candidate" \
  LLM_BENCH_PROFILE=stable-60ms \
  LLM_BENCH_RPS="$offered_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
done
