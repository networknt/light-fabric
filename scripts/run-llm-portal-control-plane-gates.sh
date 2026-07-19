#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$repo_root/.." && pwd)"
database_url="${1:-${PORTAL_LLM_TEST_DATABASE_URL:-}}"
gate_tmp="$(mktemp -d "${TMPDIR:-/tmp}/llm-portal-gate.XXXXXX")"
trap 'rm -rf -- "$gate_tmp"' EXIT

if [[ -z "$database_url" ]]; then
  echo "usage: $0 postgresql://.../DISPOSABLE_EMPTY_DATABASE" >&2
  exit 2
fi

for repo in portal-db light-portal genai-command genai-query portal-view; do
  [[ -d "$workspace_root/$repo" ]] || { echo "missing sibling repository: $repo" >&2; exit 1; }
done

echo "[llm-portal-control-plane] checking command/query endpoint publication parity"
diff -u \
  <(sed -nE 's/^- name: "([^"]*Llm[^"]*)"/\1/p' "$workspace_root/genai-command/src/main/resources/spec.yaml" "$workspace_root/genai-query/src/main/resources/spec.yaml" | sort -u) \
  <(rg -o "'(?:create|update|delete|get|validate|run|publish|rollback|preview|acknowledge)[A-Za-z0-9]*Llm[A-Za-z0-9]*'" \
      "$workspace_root/portal-db/postgres/patch_20260719_02_llm_control_plane_endpoints.sql" | tr -d "'" | sort -u)

echo "[llm-portal-control-plane] checking the shared agent eligibility contract"
cmp "$repo_root/benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.schema.json" \
    "$workspace_root/genai-query/src/test/resources/contracts/llm/v1/get-eligible-llm-models-for-agent.schema.json"
cmp "$repo_root/benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.fixture.json" \
    "$workspace_root/genai-query/src/test/resources/contracts/llm/v1/get-eligible-llm-models-for-agent.fixture.json"

echo "[llm-portal-control-plane] running PDB-1 schema/publication/access gates"
"$workspace_root/portal-db/postgres/tests/run-llm-control-plane-schema-gate.sh" "$database_url"

echo "[llm-portal-control-plane] running LP-1 persistence tests"
(cd "$workspace_root/light-portal" && mvn -q test && mvn -q -DskipTests install)

echo "[llm-portal-control-plane] running GC-1/GQ-1 contract tests"
(cd "$workspace_root/genai-command" && mvn -q test)
(cd "$workspace_root/genai-query" && mvn -q test)

echo "[llm-portal-control-plane] running gateway fixture consumer"
(cd "$repo_root" && cargo test -p llm-gateway --test local_data_plane portal_agent_eligibility_contract_is_safe_for_gateway_model_resolution)
(cd "$repo_root" && cargo test -p light-agent portal_llm_contract_tests)

echo "[llm-portal-control-plane] building PV-1"
(cd "$workspace_root/portal-view" && jq empty src/data/Forms.json \
  && npm run build -- --outDir "$gate_tmp/portal-view-dist")

echo "[llm-portal-control-plane] PASS"
