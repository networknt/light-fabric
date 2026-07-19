#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <bounded-async|local-durable> <light|bifrost> <target-url>" >&2
  exit 2
fi

profile="$1"
candidate="$2"
target="$3"
if [[ ! "$profile" =~ ^(bounded-async|local-durable)$ ]]; then
  echo "unsupported PERF-2 profile: $profile" >&2
  exit 2
fi
if [[ ! "$candidate" =~ ^(light|bifrost)$ ]]; then
  echo "unsupported PERF-2 candidate: $candidate" >&2
  exit 2
fi
if [[ "$profile" == "local-durable" && "$candidate" != "light" ]]; then
  echo "local-durable is a separately reported Light-only profile" >&2
  exit 2
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cargo run --locked -p llm-phase0-spikes -- validate-comparison "$candidate"

environment_file="${LLM_BENCH_ENVIRONMENT_FILE:-}"
metrics_dir="${LLM_BENCH_METRICS_DIR:-}"
if [[ -z "$environment_file" || -z "$metrics_dir" ]]; then
  echo "PERF-2 requires LLM_BENCH_ENVIRONMENT_FILE and LLM_BENCH_METRICS_DIR" >&2
  exit 2
fi
jq -e --arg profile "$profile" --arg candidate "$candidate" '
  .cpuLimit == "2 vCPU" and
  .memoryLimitBytes == 4294967296 and
  .keepAlive == true and
  .candidate == $candidate and
  .auditMode == ($profile | gsub("-"; "_")) and
  (if $profile == "local-durable" then .persistentStorage == true else true end)
' "$environment_file" >/dev/null

if [[ "$profile" == "bounded-async" ]]; then
  offered_rps="${LLM_BENCH_RPS:-500}"
else
  offered_rps="${LLM_BENCH_RPS:-100}"
fi
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
runs="${LLM_BENCH_RUNS:-5}"
payload="$repo_dir/benchmarks/llm-gateway/payloads/small-text.json"
report_dir="$repo_dir/benchmarks/llm-gateway/reports/perf2/$profile/$candidate"
mkdir -p "$report_dir"
cp "$environment_file" "$report_dir/environment.json"

for run in $(seq 1 "$runs"); do
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$payload" \
  LLM_BENCH_OUTPUT="$report_dir/${offered_rps}rps-run${run}.json" \
  LLM_BENCH_CANDIDATE="$candidate" \
  LLM_BENCH_PROFILE="perf2-$profile" \
  LLM_BENCH_RPS="$offered_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
  metrics_file="$metrics_dir/run${run}.json"
  jq -e '
    [.cpu, .rss, .walBytes, .walSegments, .durableWatermark,
     .fdatasyncP50, .fdatasyncP95, .fdatasyncP99,
     .commitWaitP50, .commitWaitP95, .commitWaitP99,
     .oldestUnacknowledgedAge, .sinkLag] | all(. != null)
  ' "$metrics_file" >/dev/null
  cp "$metrics_file" "$report_dir/${offered_rps}rps-run${run}-audit-metrics.json"
done
