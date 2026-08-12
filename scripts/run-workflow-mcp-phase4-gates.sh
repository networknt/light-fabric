#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
portal_db_dir="${PORTAL_DB_DIR:-$repo_dir/../portal-db}"
portal_service_dir="${LIGHT_PORTAL_DIR:-$repo_dir/../light-portal}"
genai_command_dir="${GENAI_COMMAND_DIR:-$repo_dir/../genai-command}"
genai_query_dir="${GENAI_QUERY_DIR:-$repo_dir/../genai-query}"
portal_view_dir="${PORTAL_VIEW_DIR:-$repo_dir/../portal-view}"
postgres_url="${1:-}"

# Phase 4 is control-plane-only; reuse the already-qualified Phase 3 runtime
# surface, then run the Phase 4 schema on a fresh disposable database once.
"$repo_dir/scripts/run-workflow-mcp-phase3-gates.sh"

cd "$portal_service_dir"
mvn -q -pl db-provider -am install
git diff --check

cd "$genai_command_dir"
mvn -q test
git diff --check

cd "$genai_query_dir"
mvn -q test
git diff --check

cd "$portal_view_dir"
node -e "const fs=require('fs');const forms=JSON.parse(fs.readFileSync('src/data/Forms.json','utf8'));for(const id of ['createSkillWorkflow','updateSkillWorkflow']){if(!forms[id].schema.properties.workflowBindingId)throw new Error(id+' missing workflowBindingId');if(forms[id].schema.properties.workflowToolId)throw new Error(id+' must not expose workflowToolId');}"
npx eslint src/pages/genai/SkillWorkflow.tsx src/pages/genai/SkillWorkspace.tsx
npm run build
git diff --check

if [[ -n "$postgres_url" ]]; then
  "$portal_db_dir/postgres/tests/run-workflow-mcp-phase4-schema-gate.sh" "$postgres_url"
else
  echo "Phase 4 PostgreSQL runtime gate skipped; pass a disposable PostgreSQL URL to include it."
fi

echo "Workflow-backed MCP Phase 4 implementation checks passed; no production qualification is implied."
