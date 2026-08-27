#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$repo_dir"
./scripts/run-hmac-phase2-gates.sh
cargo test --quiet --locked -p light-gateway --lib --bin light-gateway
cargo test --quiet --locked -p light-pingora hmac
cargo clippy --quiet --locked -p light-pingora -p light-gateway --all-targets --no-deps

echo "HMAC Phase 3 body gate, replay lifecycle, security snapshot, and effective-chain gates passed."
