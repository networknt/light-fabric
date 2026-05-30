#!/usr/bin/env bash
set -euo pipefail

DATABASE_URL="${DATABASE_URL:-postgresql://postgres:secret@localhost:5432/configserver}"
PORTAL_COMMAND_URL="${PORTAL_COMMAND_URL:-https://localhost:8443/portal/command}"
HOST="${HOST:-lightapi.net}"
ACCESS_TOKEN="${ACCESS_TOKEN:-}"
HOST_ID="${HOST_ID:-}"

usage() {
  cat <<'EOF'
Usage:
  ACCESS_TOKEN=<token> HOST_ID=<host-id> ./insurance-claim-demo-reset.sh

Optional:
  DATABASE_URL=postgresql://postgres:secret@localhost:5432/configserver
  PORTAL_COMMAND_URL=https://localhost:8443/portal/command
  HOST=lightapi.net

The reset emits workflow/deleteProcessInfo commands. The portal command handler
stores ProcessInfoDeletedEvent and the projection soft-deletes process_info_t by
setting active=false.
EOF
}

require_var() {
  local name="$1"
  local value="$2"
  if [[ -z "$value" ]]; then
    echo "Missing required environment variable: $name" >&2
    exit 2
  fi
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

require_var "ACCESS_TOKEN" "$ACCESS_TOKEN"
require_var "HOST_ID" "$HOST_ID"

query="
SELECT p.process_id::text,
       d.name,
       p.wf_instance_id
FROM process_info_t p
JOIN wf_definition_t d
  ON d.host_id = p.host_id
 AND d.wf_def_id = p.wf_def_id
WHERE p.host_id = :'host_id'::uuid
  AND p.active = TRUE
  AND d.name IN (
    'insurance-claim-rest-v1',
    'insurance-claim-mcp-v1',
    'insurance-claim-headless-v1'
  )
ORDER BY p.started_ts DESC;
"

rows="$(psql -X -A -t -F $'\t' "$DATABASE_URL" -v host_id="$HOST_ID" -c "$query")"

if [[ -z "$rows" ]]; then
  echo "No active insurance claim demo processes found for host $HOST_ID."
  exit 0
fi

while IFS=$'\t' read -r process_id workflow_name wf_instance_id; do
  [[ -z "$process_id" ]] && continue
  echo "Soft-deleting $workflow_name instance $wf_instance_id process $process_id"
  curl -k -sS -X POST "$PORTAL_COMMAND_URL" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $ACCESS_TOKEN" \
    -d "{
      \"host\": \"$HOST\",
      \"service\": \"workflow\",
      \"action\": \"deleteProcessInfo\",
      \"version\": \"0.1.0\",
      \"data\": {
        \"hostId\": \"$HOST_ID\",
        \"processId\": \"$process_id\"
      }
    }"
  echo
done <<< "$rows"
