#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/run-coding-harness-phase1-gates.sh

cargo test --locked -p llm-gateway --lib
cargo test --locked -p llm-gateway --test local_data_plane responses_
cargo test --locked -p llm-gateway --test local_data_plane \
  coding_worker_alias_without_current_responses_conformance_is_ineligible

cargo clippy --locked \
  -p agent-runtime-protocol \
  -p llm-gateway \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent --all-targets --no-deps
cargo fmt --check \
  -p agent-runtime-protocol \
  -p llm-gateway \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent

# The attempt token may be named only by the trusted launcher/configuration
# path. It must never be rendered into runtime events or coding artifacts.
if rg -n 'LIGHT_LLM_ATTEMPT_TOKEN' \
  crates/coding-agent-runtime/src/lib.rs; then
  echo "gateway credential environment name leaked into a durable protocol or coding artifact" >&2
  exit 1
fi

mdbook build docs
