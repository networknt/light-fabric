#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$repo_root/.." && pwd)"
database_url="${1:-${PORTAL_LLM_TEST_DATABASE_URL:-}}"

echo "[llm-production-integration] checking shared governed-alias contract"
cmp "$repo_root/benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.fixture.json" \
    "$workspace_root/genai-query/src/test/resources/contracts/llm/v1/get-eligible-llm-models-for-agent.fixture.json"

echo "[llm-production-integration] running DIST-1/LF-7 convergence, rotation, and recovery tests"
(cd "$repo_root" && cargo test --locked -p llm-gateway projection::tests)

echo "[llm-production-integration] running LA-1 governed alias contract"
(cd "$repo_root" && cargo test --locked -p light-agent shared_portal_contract_drives_governed_alias_selection)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane \
  portal_agent_eligibility_contract_is_safe_for_gateway_model_resolution)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane \
  models_never_enumerate_internal_aliases)
(cd "$workspace_root/portal-view" && npm run test:run -- \
  src/pages/genai/llm-model/validation.test.ts)

echo "[llm-production-integration] checking gateway and agent compilation"
(cd "$repo_root" && cargo check --locked -p light-gateway -p light-agent)

if [[ -n "$database_url" ]]; then
  echo "[llm-production-integration] running production integration schema gate"
  "$workspace_root/portal-db/postgres/tests/run-llm-control-plane-schema-gate.sh" "$database_url"
else
  echo "[llm-production-integration] PostgreSQL gate skipped; pass a disposable database URL to close PDB evidence"
fi

echo "[llm-production-integration] PASS"
