#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
target="${LLM_BENCH_TARGET:-http://127.0.0.1:18080/v1/chat/completions}"
duration="${LLM_BENCH_DURATION_SECONDS:-60}"
payload="$repo_dir/benchmarks/llm-gateway/payloads/small-text.json"
report_dir="$repo_dir/benchmarks/llm-gateway/reports/direct"
mkdir -p "$report_dir"

for offered_rps in 500 5000; do
  for run in 1 2 3 4 5; do
    output="$report_dir/stable-60ms-${offered_rps}rps-run${run}.json"
    LLM_BENCH_TARGET="$target" \
    LLM_BENCH_PAYLOAD="$payload" \
    LLM_BENCH_OUTPUT="$output" \
    LLM_BENCH_CANDIDATE=direct \
    LLM_BENCH_PROFILE=stable-60ms \
    LLM_BENCH_RPS="$offered_rps" \
    LLM_BENCH_DURATION_SECONDS="$duration" \
      cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
  done
done

overload_rps="${LLM_BENCH_OVERLOAD_RPS:-10000}"
for run in 1 2 3 4 5; do
  output="$report_dir/overload-${overload_rps}rps-run${run}.json"
  LLM_BENCH_TARGET="$target" \
  LLM_BENCH_PAYLOAD="$payload" \
  LLM_BENCH_OUTPUT="$output" \
  LLM_BENCH_CANDIDATE=direct \
  LLM_BENCH_PROFILE=overload \
  LLM_BENCH_RPS="$overload_rps" \
  LLM_BENCH_DURATION_SECONDS="$duration" \
    cargo run --locked --release -p llm-provider-mock --bin llm-loadgen
done
