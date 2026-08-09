#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
portal_root="${PORTAL_ROOT:-$repo_root/../light-portal}"
db_root="${PORTAL_DB_ROOT:-$repo_root/../portal-db}"
command_root="${GENAI_COMMAND_ROOT:-$repo_root/../genai-command}"
query_root="${GENAI_QUERY_ROOT:-$repo_root/../genai-query}"
view_root="${PORTAL_VIEW_ROOT:-$repo_root/../portal-view}"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
mkdir -p "$report_dir"

for required in "$portal_root" "$db_root" "$command_root" "$query_root" "$view_root"; do
  test -d "$required/.git"
done
: "${LMT_DATABASE_URL:?LMT_DATABASE_URL must name a disposable PostgreSQL database}"

(cd "$repo_root" && cargo test --locked -p llm-gateway)
(cd "$portal_root" && mvn -q test && mvn -q -DskipTests install)
(cd "$command_root" && mvn -q test)
(cd "$query_root" && mvn -q test)
(cd "$db_root" && ./postgres/tests/run-llm-control-plane-schema-gate.sh "$LMT_DATABASE_URL")
(cd "$view_root" && npm test -- --run src/pages/genai/llm-model)

jq -n --arg gate "LMT-G1" --arg status "PASS" \
  --arg lightFabric "$(git -C "$repo_root" rev-parse HEAD)" \
  --arg portalDb "$(git -C "$db_root" rev-parse HEAD)" \
  --arg lightPortal "$(git -C "$portal_root" rev-parse HEAD)" \
  --arg genaiCommand "$(git -C "$command_root" rev-parse HEAD)" \
  --arg genaiQuery "$(git -C "$query_root" rev-parse HEAD)" \
  --arg portalView "$(git -C "$view_root" rev-parse HEAD)" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevisions:{lightFabric:$lightFabric,portalDb:$portalDb,lightPortal:$lightPortal,genaiCommand:$genaiCommand,genaiQuery:$genaiQuery,portalView:$portalView},assertions:["v2_v3_dual_reader","v3_local_enablement_gate","endpoint_zone_storage","signed_evidence_admission","per_replica_acknowledgement","authenticated_outbox_forwarder","zero_price_ledger","portal_forms_read_only_evidence"]}' \
  > "$report_dir/lmt-g1-admission.json"
echo "LMT-G1 PASS: $report_dir/lmt-g1-admission.json"

