#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo test --locked -p llm-gateway compiler_resolves_secrets_and_clients_off_path_and_reuses_deployments
cargo test --locked -p llm-gateway invalid_candidate_is_not_published_and_retirement_is_bounded
cargo test --locked -p light-gateway llm_handler_is_registered_but_disabled_path_does_no_config_or_secret_work

echo "[llm-request-path-invariants] PASS"

