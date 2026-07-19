#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
database_url="${1:-${LLM_AUDIT_TEST_DATABASE_URL:-}}"
migration="$repo_root/crates/llm-gateway/migrations/audit-postgres/0001_llm_audit.sql"

echo "[llm-audit-wal] durability, recovery, corruption, capacity, replay"
(cd "$repo_root" && cargo test --locked -p llm-gateway audit::tests --lib)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane local_durable_)

echo "[llm-audit-wal] dedicated metadata-only audit schema"
for table in llm_audit_event_t llm_request_t llm_attempt_t llm_content_object_t llm_dataset_export_t; do
  rg -q "CREATE TABLE IF NOT EXISTS $table" "$migration"
done
for role in llm_audit_gateway_ingest llm_audit_auditor_read llm_audit_retention llm_audit_dataset_export; do
  rg -q "CREATE ROLE $role NOLOGIN" "$migration"
done
if sed '/^COMMENT ON TABLE/,$d' "$migration" | \
  rg -qi 'prompt|completion|tool_argument|credential|provider_error_body|reversible_pii'; then
  echo "audit schema contains a forbidden content column" >&2
  exit 1
fi

if [[ -n "$database_url" ]]; then
  echo "[llm-audit-wal] applying schema twice and testing duplicate replay"
  psql "$database_url" -v ON_ERROR_STOP=1 -f "$migration"
  psql "$database_url" -v ON_ERROR_STOP=1 -f "$migration"
  psql "$database_url" -v ON_ERROR_STOP=1 -Atc \
    "SELECT count(*) FROM pg_class WHERE relname IN ('llm_audit_event_t','llm_request_t','llm_attempt_t','llm_content_object_t','llm_dataset_export_t')" | rg -qx '5'
  (cd "$repo_root" && LLM_AUDIT_TEST_DATABASE_URL="$database_url" \
    cargo test --locked -p llm-gateway audit::tests::postgres_sink_duplicate_delivery_is_idempotent_when_database_is_available --lib)
else
  echo "[llm-audit-wal] PostgreSQL replay smoke skipped; pass a disposable dedicated audit database URL"
fi

echo "[llm-audit-wal] PASS"
