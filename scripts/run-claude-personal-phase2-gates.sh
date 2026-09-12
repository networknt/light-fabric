#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
./scripts/run-claude-personal-phase1-gates.sh
rustfmt --edition 2024 --check --config skip_children=true \
    apps/light-agent-worker/src/claude_code/coding.rs \
    apps/light-agent-worker/src/claude_code/coding/tests.rs \
    apps/light-agent-worker/src/claude_code/native_namespace.rs \
    apps/light-agent-worker/src/claude_code/runtime.rs \
    crates/coding-agent-runtime/src/claude.rs \
    apps/light-agent/src/claude_admission.rs \
    apps/light-workflow-runner/src/claude_configuration.rs \
    apps/light-agent-worker/examples/claude-coding.rs
cargo build --locked -p light-agent-worker --features claude-prototype --bins --example claude-coding
cargo build --locked -p light-workflow-runner --example claude-dispatch
cargo test --locked -p light-agent --bin light-agent
cargo test --locked -p light-workflow-runner --lib
cargo test --locked -p light-agent --lib
./target/debug/light-claude-worker print-capabilities |
    python3 -c 'import json,sys; c=json.load(sys.stdin)["capabilities"]; assert c["actions"] == ["coding.claude-code-v1"]; assert not c["supportsApprovals"] and not c["supportsUsage"]' 
python3 -m py_compile scripts/run-claude-coding-smoke.py
if [[ "${LIGHT_RUN_CLAUDE_CODING_SMOKE:-0}" == 1 ]]; then
    python3 scripts/run-claude-coding-smoke.py \
        --claude "${LIGHT_CLAUDE_EXECUTABLE:?set the pinned native CLI path}" \
        --native-home "${LIGHT_CLAUDE_HOME:?set the existing native configuration directory}" \
        --worker target/debug/light-claude-worker \
        --runner target/debug/examples/claude-dispatch \
        --report "${LIGHT_CLAUDE_PHASE2_REPORT:-/tmp/claude-phase2-live.json}"
fi
echo 'Claude Phase 2 integration gates passed; local technical qualification is separate from distribution eligibility.'
