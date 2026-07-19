#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
evidence_dir="$repo_dir/benchmarks/llm-gateway/evidence"
source_commit="$(git -C "$repo_dir" rev-parse HEAD)"
rustc_version="$(rustc --version)"

export PHASE0_SOURCE_COMMIT="$source_commit"
export PHASE0_RUSTC="$rustc_version"

cargo run --locked --release -p llm-phase0-spikes -- body "$evidence_dir/body-capture.json"
cargo run --locked --release -p llm-phase0-spikes -- snapshot "$evidence_dir/snapshot.json"
cargo run --locked --release -p llm-phase0-spikes -- projection-secret "$evidence_dir/projection-secret.json"
cargo run --locked --release -p llm-phase0-spikes -- wal "$evidence_dir/wal.json"

