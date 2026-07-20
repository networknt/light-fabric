#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:---implementation}"
case "$mode" in
  --implementation|--functional|--security|--durability|--performance|--promote) ;;
  *) echo "usage: $0 [--implementation|--functional|--security|--durability|--performance|--promote]" >&2; exit 2 ;;
esac

echo "[llm-pii] typed request/response/stream and eligibility contracts"
jq empty "$repo_root/operations/llm-gateway/pii-promotion.json"
jq empty "$repo_root/benchmarks/llm-gateway/manifests/perf4-manifest.json"
jq empty "$repo_root/benchmarks/llm-gateway/schemas/pii-promotion-evidence.schema.json"
bash -n "$0"
bash -n "$repo_root/benchmarks/llm-gateway/scripts/run-perf4-profile.sh"
(cd "$repo_root" && cargo test --locked -p llm-gateway pii::tests)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane request_scoped_pii)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane reject_buffered_pii)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane pii_alias_requires_placeholder_preservation_evidence)

if [[ -n "${LLM_PII_VAULT_DATABASE_URL:-}" ]]; then
  echo "[llm-pii] disposable dedicated-vault schema and exact-role smoke"
  migration="$repo_root/crates/llm-gateway/migrations/pii-vault-postgres/0001_pii_vault.sql"
  psql "$LLM_PII_VAULT_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$migration" >/dev/null
  psql "$LLM_PII_VAULT_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$migration" >/dev/null
  psql "$LLM_PII_VAULT_DATABASE_URL" -v ON_ERROR_STOP=1 <<'SQL' >/dev/null
SET ROLE llm_pii_vault_gateway;
SELECT llm_pii_vault_insert_exact(
  repeat('a',64)::char(64), repeat('b',64)::char(64), repeat('c',64)::char(64),
  decode('0102','hex'), 'kms://implementation-smoke/key', clock_timestamp() + interval '1 hour'
);
SELECT encrypted_value,key_reference FROM llm_pii_vault_resolve_exact(
  repeat('a',64)::char(64), repeat('b',64)::char(64), repeat('c',64)::char(64)
);
SELECT llm_pii_vault_revoke_exact(
  repeat('a',64)::char(64), repeat('b',64)::char(64), repeat('c',64)::char(64)
);
SQL
  if psql "$LLM_PII_VAULT_DATABASE_URL" -v ON_ERROR_STOP=1 \
      -c "SET ROLE llm_pii_vault_gateway; SELECT encrypted_value FROM llm_pii_vault_entry_t" \
      >/dev/null 2>&1; then
    echo "gateway role unexpectedly received PII vault scan access" >&2
    exit 1
  fi
else
  echo "[llm-pii] PostgreSQL vault smoke skipped; pass LLM_PII_VAULT_DATABASE_URL for a disposable dedicated database"
fi

command="validate-pii-implementation"
case "$mode" in
  --functional) command="validate-pii-functional" ;;
  --security) command="validate-pii-security" ;;
  --durability) command="validate-pii-durability" ;;
  --performance) command="validate-perf4" ;;
  --promote) command="validate-pii-promotion" ;;
esac
(cd "$repo_root" && cargo run --locked -p llm-phase0-spikes -- "$command")

echo "[llm-pii] PASS ($mode)"
