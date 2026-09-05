#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/run-coding-harness-phase2-gates.sh

cargo test --locked -p coding-agent-runtime --lib \
  immutable_role_profiles_review_reconstruction_and_publish_gate_fail_closed
cargo test --locked -p light-agent-worker --lib reviewer_
cargo test --locked -p light-github-action-provider --bin light-github-action-provider \
  lost_branch_response_reconciles_then_opens_pr_at_patched_commit
cargo test --locked -p light-workflow-runner --lib

cargo clippy --locked \
  -p coding-agent-runtime \
  -p light-agent-worker \
  -p light-agent \
  -p light-workflow-runner \
  -p light-github-action-provider --all-targets --no-deps
cargo fmt --check \
  -p coding-agent-runtime \
  -p light-agent-worker \
  -p light-agent \
  -p light-workflow-runner \
  -p light-github-action-provider

mdbook build docs
