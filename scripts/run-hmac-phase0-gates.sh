#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$repo_dir"
cargo fmt --all --check
cargo test --quiet --locked -p hmac-phase0-spikes
cargo clippy --quiet --locked -p hmac-phase0-spikes --all-targets --no-deps -- -D warnings
mdbook build docs

echo "HMAC Phase 0 passed: the gateway-core prebuffer hook replays the required 16 MiB body."
