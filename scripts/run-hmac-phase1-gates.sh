#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$repo_dir"
./scripts/run-hmac-phase0-gates.sh
cargo test --quiet --locked -p light-pingora
cargo test --quiet --locked -p light-gateway
cargo clippy --quiet --locked -p light-pingora -p light-gateway --all-targets --no-deps

echo "HMAC Phase 1 configuration, policy, verifier, startup, and reload gates passed."
