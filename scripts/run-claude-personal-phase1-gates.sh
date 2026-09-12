#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
./scripts/run-claude-personal-phase0-gates.sh
rustfmt --edition 2024 --check --config skip_children=true \
    apps/light-agent-worker/src/claude_code.rs \
    apps/light-agent-worker/src/claude_code/tests.rs \
    apps/light-agent-worker/src/coding_session.rs
cargo test --locked -p light-agent-worker -p coding-agent-runtime --lib --features light-agent-worker/claude-prototype
cargo check --locked -p light-agent-worker --no-default-features
# Enabling the candidate library must not alter the executable's admitted adapter.
cargo run --locked --quiet -p light-agent-worker --features claude-prototype -- print-capabilities |
    python3 -c 'import json,sys; c=json.load(sys.stdin)["capabilities"]; assert c["adapterId"] == "codex-app-server-v1"; assert "coding.claude-code-v1" not in c["actions"]'
if rg -n 'codex-embedded-v1' \
    --glob '!claude_code.rs' --glob '!**/claude_code/**' \
    apps/light-agent-worker/src apps/light-agent/src; then
    echo 'Embedded prototype referenced from production selection code' >&2
    exit 1
fi
mdbook build docs --dest-dir "${LIGHT_CLAUDE_DOC_OUTPUT:-/tmp/light-fabric-claude-phase1-book}"
echo 'Claude Phase 1 deterministic adapter gate passed; the default worker remains Codex-only.'
