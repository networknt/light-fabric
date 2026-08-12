#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow_query_dir="${WORKFLOW_QUERY_DIR:-$repo_dir/../workflow-query}"
workflow_command_dir="${WORKFLOW_COMMAND_DIR:-$repo_dir/../workflow-command}"
portal_view_dir="${PORTAL_VIEW_DIR:-$repo_dir/../portal-view}"
postgres_url="${1:-}"

"$repo_dir/scripts/run-workflow-mcp-phase2-gates.sh" "$postgres_url"

cd "$workflow_query_dir"
mvn -q test
git diff --check

cd "$workflow_command_dir"
mvn -q test
git diff --check

cd "$portal_view_dir"
npm run test:run -- src/pages/workflow/WorkflowAiAuthoringDialog.test.ts
npx eslint src/pages/workflow/WorkflowEditor.tsx \
  src/pages/workflow/WorkflowAiAuthoringDialog.tsx \
  src/pages/workflow/WorkflowAiAuthoringDialog.test.ts \
  src/pages/workflow/workflowAiAuthoring.ts
npm run build
git diff --check

echo "Workflow-backed MCP Phase 3 implementation checks passed; authoring remains development-only."
