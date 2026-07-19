#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo test --locked -p llm-gateway buffered_security_denies_before_json_and_alias_parse
cargo test --locked -p llm-gateway buffered_response_uses_trusted_id_and_hides_physical_provider_evidence
cargo test --locked -p llm-gateway buffered_http_rejects_method_media_size_and_operated_field_conflicts
cargo test --locked -p llm-gateway mixed_format_alias_parses_for_the_eligible_provider_set
cargo test --locked -p llm-gateway models_never_enumerate_internal_aliases
cargo test --locked -p llm-gateway buffered_errors_preserve_retry_after_and_use_client_fault_message
cargo test --locked -p llm-gateway partial_usage_keeps_total_tokens_unknown

echo "[llm-body-contract] PASS"
