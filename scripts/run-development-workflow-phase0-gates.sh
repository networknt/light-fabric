#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt -p development-workflow-contract --check
cargo test --locked -p development-workflow-contract
cargo clippy --locked -p development-workflow-contract --all-targets -- -D warnings
git diff --check
printf '%s\n' 'Development workflow Phase 0 contracts passed (no model calls or deployment).'
