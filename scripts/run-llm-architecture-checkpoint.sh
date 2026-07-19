#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

mode="${1:---closure}"
if [[ "$mode" != "--closure" && "$mode" != "--implementation" ]]; then
  echo "usage: $0 [--closure|--implementation]" >&2
  exit 2
fi

./scripts/run-llm-buffered-gate.sh
cargo test --locked -p llm-gateway --test local_data_plane early_sse_
cargo test --locked -p model-provider cancellation_reaches_mock_after_first_output
cargo test --locked -p light-gateway llm_sse_smoke_streams_openai_frames_over_live_pingora

if [[ "$mode" == "--implementation" ]]; then
  cargo run --locked -p llm-phase0-spikes -- validate-perf1-implementation
  echo "[llm-architecture-checkpoint] implementation PASS; PERF-1 closure evidence still required"
else
  cargo run --locked -p llm-phase0-spikes -- validate-perf1
  echo "[llm-architecture-checkpoint] PASS"
fi
