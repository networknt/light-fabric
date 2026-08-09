#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
mkdir -p "$report_dir"

(cd "$repo_root" && cargo test --locked -p llm-gateway provider::tests)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane local_profile_rejection_precedes_legacy_development_fixture_validation)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane credential_free_plaintext_never_calls_the_secret_resolver)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane compiler_rejects_shared_capacity_between_query_and_index_lanes)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane ambiguous_usage_is_conservatively_nonzero_and_incomplete)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test contracts_v3)

jq -n --arg gate "LMT-G2" --arg status "PASS" --arg revision "$(git -C "$repo_root" rev-parse HEAD)" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevision:$revision,assertions:["no_proxy","redirects_disabled","zone_filtered_dns","peer_address_revalidated","url_host_is_sni","private_ca","trust_digest_client_identity","credential_free_plaintext"]}' \
  > "$report_dir/lmt-g2-client.json"
echo "LMT-G2 PASS: $report_dir/lmt-g2-client.json"
