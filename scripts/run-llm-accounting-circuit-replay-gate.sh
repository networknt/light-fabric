#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo test --locked -p llm-gateway lf5_single_attempt_never_uses_fallback_and_finalizes_audit
cargo test --locked -p llm-gateway lf5b_safe_failure_falls_back_once_and_reconciles_exact_usage
cargo test --locked -p llm-gateway mandatory_retry_rejects_oversize_replay_before_dispatch
cargo test --locked -p llm-gateway ambiguous_usage_is_conservatively_nonzero_and_incomplete
cargo test --locked -p llm-gateway passive_circuit_allows_only_one_half_open_probe
cargo test --locked -p llm-gateway half_open_probe_non_circuit_failure_releases_probe_slot

echo "[llm-accounting-circuit-replay] PASS"
