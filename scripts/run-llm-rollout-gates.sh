#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
require_live=false
if [[ "${1:-}" == "--require-live-evidence" ]]; then
  require_live=true
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--require-live-evidence]" >&2
  exit 2
fi

echo "[llm-rollout] REL-1 contracts and rollback behavior"
jq empty "$repo_root/operations/llm-gateway/rollout-plan.json"
jq empty "$repo_root/benchmarks/llm-gateway/schemas/rel1-canary-evidence.schema.json"
jq empty "$repo_root/benchmarks/llm-gateway/schemas/rel1-rollback-evidence.schema.json"
(cd "$repo_root" && cargo test --locked -p llm-gateway projection::tests::rollback_republishes_prior_resources_at_a_new_monotonic_sequence)
(cd "$repo_root" && cargo test --locked -p llm-gateway audit::sink::tests::explicit_stop_aborts_the_audit_sink_worker)
(cd "$repo_root" && cargo test --locked -p llm-gateway audit::tests::in_flight_reservation_retains_writer_lock_until_terminal_audit_drains)
(cd "$repo_root" && cargo test --locked -p light-gateway disabling_llm_module_stops_the_existing_projection_worker)

if [[ "$require_live" == true ]]; then
  (cd "$repo_root" && cargo run --locked -p llm-phase0-spikes -- validate-rollout)
else
  (cd "$repo_root" && cargo run --locked -p llm-phase0-spikes -- validate-rollout-implementation)
fi

echo "[llm-rollout] PASS"
