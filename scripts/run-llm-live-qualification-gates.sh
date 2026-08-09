#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
mkdir -p "$report_dir"

(cd "$repo_root" && cargo test --locked -p provider-qualification-runner)
(cd "$repo_root" && cargo test --locked -p model-provider conformance::live)
(cd "$repo_root" && cargo test --locked -p llm-gateway runtime::readiness)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test contracts_v3)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane compiler_rejects_shared_capacity_between_query_and_index_lanes)

jq -n --arg gate "LMT-G4" --arg status "PASS" --arg revision "$(git -C "$repo_root" rev-parse HEAD)" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevision:$revision,networkAssertion:"deferred_to_lmt_g5_cluster_exercise",assertions:["signed_live_endpoint_binding","unknown_key_rejected","external_vantage_metadata","isolation_manifest_required","operation_independence","cold_start_timing","qualified_parallelism","queue_saturation","request_and_stream_timeout_bounds","stream_disconnect_recovery","embedding_format_and_dimension","capacity_domain_isolation","warm_before_eligible"]}' \
  > "$report_dir/lmt-g4-live-qualification.json"
echo "LMT-G4 PASS: $report_dir/lmt-g4-live-qualification.json"
