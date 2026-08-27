#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$repo_dir"
./scripts/run-hmac-phase1-gates.sh
cargo test --quiet --locked -p light-runtime
cargo test --quiet --locked -p light-pingora hmac_replay
cargo test --quiet --locked -p light-pingora hmac::tests
cargo test --quiet --locked -p light-gateway replay_admin_tool
cargo clippy --quiet --locked -p light-runtime -p light-pingora -p light-gateway --all-targets --no-deps

echo "HMAC Phase 2 replay-store, cache-summary, reload-preservation, and administration gates passed."
