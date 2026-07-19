#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
result_dir="$(mktemp -d)"
trap 'rm -rf -- "$result_dir"' EXIT

cd "$repo_dir"
cargo test --locked -p model-provider
cargo test --locked -p light-agent --lib
cargo test --locked -p light-workflow --lib
cargo run --locked -p model-provider --bin provider-conformance -- \
  --corpus crates/model-provider/conformance/v1 \
  --expected crates/model-provider/conformance/results \
  --output "$result_dir" \
  --as-of 2026-07-19T00:00:00Z

echo "LF-3/LF-4 provider contract and conformance gates passed"
