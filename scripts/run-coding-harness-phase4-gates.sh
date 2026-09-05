#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/run-coding-harness-phase3-gates.sh

cargo test --locked -p agent-runtime-protocol --lib \
  attempt_credentials_require_live_versioned_unrevoked_generation
cargo test --locked -p light-agent-worker --lib authentication_
cargo test --locked -p light-workflow-runner --lib \
  authentication_profiles_require_separate_pools_and_exact_user_tenant_binding
cargo test --locked -p light-workflow-runner --lib \
  attempt_credential_delivery_requires_exact_audience_and_binding
cargo test --locked -p light-agent --bin light-agent \
  coding_profile_accepts_only_pinned_codex_app_server_adapter

cargo clippy --locked \
  -p agent-runtime-protocol \
  -p coding-agent-runtime \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent --all-targets --no-deps
cargo fmt --check \
  -p agent-runtime-protocol \
  -p coding-agent-runtime \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent

# Durable authentication evidence is a closed metadata-only shape. Vendor
# account details and secret-bearing field names must never enter that shape.
if sed -n '/pub struct CodingAuthenticationEvidence/,/^}/p' \
  crates/coding-agent-runtime/src/lib.rs | \
  rg -n 'token|secret|email|plan|account'; then
  echo "secret or vendor-account material entered coding authentication evidence" >&2
  exit 1
fi

rg -q 'portal-config-loc/all-in-lt' \
  docs/src/product/light-agent/coding-harness-integration.md
rg -q 'light-portal-install' \
  docs/src/product/light-agent/coding-harness-integration.md

mdbook build docs
