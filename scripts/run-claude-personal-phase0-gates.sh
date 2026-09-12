#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
python3 -m unittest discover -s prototypes/claude-code-v1 -p 'test_*.py'
if [[ "${LIGHT_RUN_CLAUDE_PERSONAL_SMOKE:-0}" == "1" ]]; then
    python3 prototypes/claude-code-v1/phase0.py \
        --claude "${LIGHT_CLAUDE_EXECUTABLE:?set absolute path to pinned Claude CLI}" \
        --model "${LIGHT_CLAUDE_NATIVE_MODEL:-sonnet}" \
        --report "${LIGHT_CLAUDE_PHASE0_REPORT:?set a private report output path}" --live
else
    echo 'Offline Phase 0 tests passed; native feasibility NOT RUN (not a live qualification pass).'
fi
