#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:---closure}"

cd "$repo_dir"
cargo test --locked -p llm-provider-mock
cargo test --locked -p llm-phase0-spikes
cargo run --locked -p llm-phase0-spikes -- validate

if [[ "$mode" != "--closure" && "$mode" != "--implementation" ]]; then
  echo "usage: $0 [--implementation|--closure]" >&2
  exit 2
fi
if [[ "$mode" == "--closure" ]]; then
  cargo run --locked -p llm-phase0-spikes -- validate-closure
fi

echo "LLM Phase 0 $mode gates passed"
