#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <light|bifrost|agentgateway> <target-url>" >&2
  exit 2
fi

candidate="$1"
target="$2"
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
descriptor="$repo_dir/benchmarks/llm-gateway/manifests/candidates/$candidate.json"
[[ -f "$descriptor" ]] || { echo "missing pinned descriptor: $descriptor" >&2; exit 1; }
grep -Eq '"revision": "[0-9a-f]{40}"' "$descriptor" || {
  echo "candidate is not pinned by a full commit" >&2
  exit 1
}
cargo run --locked -p llm-phase0-spikes -- validate-comparison "$candidate"

payload="$repo_dir/benchmarks/llm-gateway/payloads/small-text.json"
report_dir="$repo_dir/benchmarks/llm-gateway/reports/$candidate"
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
offered_rps="${LLM_BENCH_RPS:-500}"
profile="${LLM_BENCH_PROFILE:-stable-60ms}"
mkdir -p "$report_dir"

for run in 1 2 3 4 5; do
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$payload" \
  LLM_BENCH_OUTPUT="$report_dir/${offered_rps}rps-run${run}.json" \
  LLM_BENCH_CANDIDATE="$candidate" \
  LLM_BENCH_PROFILE="$profile" \
  LLM_BENCH_RPS="$offered_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
done
