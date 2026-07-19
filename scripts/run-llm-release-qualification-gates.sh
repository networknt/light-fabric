#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
require_performance=false
if [[ "${1:-}" == "--require-performance" ]]; then
  require_performance=true
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--require-performance]" >&2
  exit 2
fi

echo "[llm-release] PERF-3/OBS-1/SEC-1 artifact contracts"
jq empty "$repo_root/benchmarks/llm-gateway/manifests/perf3-manifest.json"
jq empty "$repo_root/operations/llm-gateway/metrics-contract.json"
jq empty "$repo_root/operations/llm-gateway/dashboards.json"
jq empty "$repo_root/operations/llm-gateway/alerts.json"
jq empty "$repo_root/operations/llm-gateway/synthetic-triggers.json"
jq empty "$repo_root/security/llm-gateway/threat-model.json"
jq empty "$repo_root/security/llm-gateway/evidence.json"
bash -n "$repo_root/benchmarks/llm-gateway/scripts/run-perf3-candidate.sh"

echo "[llm-release] SEC-1 provider boundary and request-path tests"
(cd "$repo_root" && cargo test --locked -p llm-gateway provider::tests)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane buffered_security_)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane models_never_enumerate_internal_aliases)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane internal_alias_invocation_is_bound_to_its_approved_principal)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane credential_rotation_rebuilds_client_but_preserves_provider_account_runtime)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane buffered_errors_preserve_retry_after_and_use_client_fault_message)
(cd "$repo_root" && cargo test --locked -p llm-gateway audit::tests::audit_event_is_metadata_only_and_hashes_the_principal)
(cd "$repo_root" && cargo test --locked -p llm-gateway projection::tests::projection_path_components_reject_directory_traversal)
(cd "$repo_root" && cargo test --locked -p light-gateway llm_handler_requires_body_aware_access_control_proof)

if [[ "$require_performance" == true ]]; then
  (cd "$repo_root" && cargo run --locked -p llm-phase0-spikes -- validate-release)
else
  (cd "$repo_root" && cargo run --locked -p llm-phase0-spikes -- validate-release-implementation)
fi

echo "[llm-release] PASS"
