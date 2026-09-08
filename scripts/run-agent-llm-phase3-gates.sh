#!/usr/bin/env bash
set -euo pipefail
fabric_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
view_root="${PORTAL_VIEW_SOURCE_DIR:-$(dirname "$fabric_root")/portal-view}"
: "${LIGHT_AGENT_TEST_DATABASE_URL:?Set a disposable PostgreSQL database with operational and agent migrations applied}"
export LIGHT_AGENT_TEST_SCHEMA=agent_ops
cd "$fabric_root"
psql "$LIGHT_AGENT_TEST_DATABASE_URL" -v ON_ERROR_STOP=1 -c "DO \$\$ BEGIN IF to_regclass('agent_ops.agent_turn_t') IS NULL THEN RAISE EXCEPTION 'Apply operational metadata and Agent store migrations first'; END IF; END \$\$;"
cargo test --locked -p agent-runtime-protocol gateway_delegation
cargo test --locked -p model-provider --lib
cargo test --locked -p model-provider --test gateway_authorization
cargo test --locked -p light-agent --lib --bin light-agent
cargo test --locked -p light-agent --lib durable_admission_is_idempotent_fifo_and_projection_rebuildable -- --ignored
cargo test --locked -p light-agent --lib reconciliation_expires_sessions_when_execution_is_unavailable -- --ignored
cargo test --locked -p llm-gateway --test local_data_plane
mdbook build docs
git diff --check
cd "$view_root"
./node_modules/.bin/eslint src/pages/genai/Chat.tsx src/pages/genai/chatAuthentication.ts src/pages/genai/chatTurns.ts src/pages/genai/Chat.test.tsx
./node_modules/.bin/vitest run src/pages/genai/Chat.test.tsx
git diff --check
echo 'Agent LLM Phase 3 gates passed: signed-token renewal, wire isolation, PostgreSQL session boundaries, and UI reconnect.'
