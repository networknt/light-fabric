#!/usr/bin/env bash
set -euo pipefail

PORTAL_COMMAND_URL="${PORTAL_COMMAND_URL:-https://localhost:8443/portal/command}"
HOST="${HOST:-lightapi.net}"
ACCESS_TOKEN="${ACCESS_TOKEN:-}"
HOST_ID="${HOST_ID:-}"
REST_WF_DEF_ID="${REST_WF_DEF_ID:-}"
MCP_WF_DEF_ID="${MCP_WF_DEF_ID:-}"
HEADLESS_WF_DEF_ID="${HEADLESS_WF_DEF_ID:-}"
TASK_ID="${TASK_ID:-}"
TASK_ASST_ID="${TASK_ASST_ID:-}"

usage() {
  cat <<'EOF'
Usage:
  ACCESS_TOKEN=<token> HOST_ID=<host-id> REST_WF_DEF_ID=<wf-id> ./insurance-claim-demo-curl.sh start-rest
  ACCESS_TOKEN=<token> HOST_ID=<host-id> MCP_WF_DEF_ID=<wf-id> ./insurance-claim-demo-curl.sh start-mcp
  ACCESS_TOKEN=<token> HOST_ID=<host-id> HEADLESS_WF_DEF_ID=<wf-id> ./insurance-claim-demo-curl.sh start-headless
  ACCESS_TOKEN=<token> HOST_ID=<host-id> TASK_ASST_ID=<task-asst-id> ./insurance-claim-demo-curl.sh claim-task
  ACCESS_TOKEN=<token> HOST_ID=<host-id> TASK_ID=<task-id> TASK_ASST_ID=<task-asst-id> ./insurance-claim-demo-curl.sh complete-adjuster-approved
  ACCESS_TOKEN=<token> HOST_ID=<host-id> TASK_ID=<task-id> TASK_ASST_ID=<task-asst-id> ./insurance-claim-demo-curl.sh complete-claimant-accepted

Optional:
  PORTAL_COMMAND_URL=https://localhost:8443/portal/command
  HOST=lightapi.net
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

post_command() {
  local body="$1"
  require_var "ACCESS_TOKEN" "$ACCESS_TOKEN"
  curl -k -sS -X POST "$PORTAL_COMMAND_URL" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $ACCESS_TOKEN" \
    -d "$body"
  echo
}

claim_input() {
  cat <<'JSON'
{
  "customerId": "CUST-1001",
  "vehicleId": "VEH-1001",
  "incidentDate": "2026-05-30",
  "accidentDescription": "Rear-ended at an intersection. No injuries reported.",
  "location": "Ottawa, ON",
  "injuryReported": false,
  "vehicleDrivable": false,
  "policeReportFiled": true,
  "photosAvailable": true,
  "channel": "portal"
}
JSON
}

headless_input() {
  cat <<'JSON'
{
  "customerId": "CUST-1001",
  "vehicleId": "VEH-1001",
  "incidentDate": "2026-05-30",
  "accidentDescription": "Rear-ended at an intersection. No injuries reported.",
  "location": "Ottawa, ON",
  "injuryReported": false,
  "vehicleDrivable": false,
  "policeReportFiled": true,
  "photosAvailable": true,
  "channel": "portal",
  "simulateMissingInfo": false,
  "missingFields": [],
  "approvalMode": "auto-approve",
  "siuMode": "clear",
  "customerResponseMode": "accept-repair"
}
JSON
}

start_workflow() {
  local wf_def_id="$1"
  local input_json="$2"
  require_var "HOST_ID" "$HOST_ID"
  require_var "workflow definition id" "$wf_def_id"
  post_command "{
    \"host\": \"$HOST\",
    \"service\": \"workflow\",
    \"action\": \"startWorkflow\",
    \"version\": \"0.1.0\",
    \"data\": {
      \"hostId\": \"$HOST_ID\",
      \"wfDefId\": \"$wf_def_id\",
      \"input\": $input_json
    }
  }"
}

claim_task() {
  require_var "HOST_ID" "$HOST_ID"
  require_var "TASK_ASST_ID" "$TASK_ASST_ID"
  post_command "{
    \"host\": \"$HOST\",
    \"service\": \"workflow\",
    \"action\": \"claimHumanTask\",
    \"version\": \"0.1.0\",
    \"data\": {
      \"hostId\": \"$HOST_ID\",
      \"taskAsstId\": \"$TASK_ASST_ID\",
      \"claimMinutes\": 30
    }
  }"
}

complete_task() {
  local value="$1"
  local comment="$2"
  require_var "HOST_ID" "$HOST_ID"
  require_var "TASK_ID" "$TASK_ID"
  require_var "TASK_ASST_ID" "$TASK_ASST_ID"
  post_command "{
    \"host\": \"$HOST\",
    \"service\": \"workflow\",
    \"action\": \"completeTask\",
    \"version\": \"0.1.0\",
    \"data\": {
      \"hostId\": \"$HOST_ID\",
      \"taskId\": \"$TASK_ID\",
      \"taskAsstId\": \"$TASK_ASST_ID\",
      \"statusCode\": \"C\",
      \"response\": {
        \"value\": \"$value\",
        \"comment\": \"$comment\"
      }
    }
  }"
}

command="${1:-}"
case "$command" in
  start-rest)
    start_workflow "$REST_WF_DEF_ID" "$(claim_input)"
    ;;
  start-mcp)
    start_workflow "$MCP_WF_DEF_ID" "$(claim_input)"
    ;;
  start-headless)
    start_workflow "$HEADLESS_WF_DEF_ID" "$(headless_input)"
    ;;
  claim-task)
    claim_task
    ;;
  complete-adjuster-approved)
    complete_task "APPROVED" "Phase 6 demo adjuster approval."
    ;;
  complete-claimant-accepted)
    complete_task "ACCEPT_REPAIR" "Phase 6 demo claimant accepted repair."
    ;;
  *)
    usage
    exit 2
    ;;
esac
