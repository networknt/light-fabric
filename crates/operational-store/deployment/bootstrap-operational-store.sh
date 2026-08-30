#!/usr/bin/env bash
set -euo pipefail

database_name="${PORTAL_DB_OPERATIONAL_NAME:-operations}"
database_user="${POSTGRES_USER:-postgres}"
bundle_root="${OPERATIONAL_BUNDLE_ROOT:-/opt/operational-store/bundle}"
secret_file="${OPERATIONAL_DATABASE_URL_FILE:-/run/secrets/operational-database-url}"
execution_secret_file="${EXECUTION_DATABASE_URL_FILE:-/run/secrets/execution-database-url}"
workflow_secret_file="${WORKFLOW_DATABASE_URL_FILE:-/run/secrets/workflow-database-url}"
a2a_secret_file="${A2A_DATABASE_URL_FILE:-/run/secrets/a2a-database-url}"
gateway_secret_file="${GATEWAY_DATABASE_URL_FILE:-/run/secrets/gateway-database-url}"
audit_secret_file="${AUDIT_DATABASE_URL_FILE:-/run/secrets/audit-database-url}"
artifact_secret_file="${ARTIFACT_DATABASE_URL_FILE:-/run/secrets/artifact-database-url}"
binding_id="${OPERATIONAL_BINDING_ID:-}"
binding_digest="${OPERATIONAL_BINDING_DIGEST:-}"
scope_id="${OPERATIONAL_SCOPE_ID:-}"
host_id="${OPERATIONAL_HOST_ID:-}"
environment_name="${OPERATIONAL_ENVIRONMENT:-}"
deployment_profile="${OPERATIONAL_DEPLOYMENT_PROFILE:-DEV_DEDICATED}"
contract_generation="${OPERATIONAL_CONTRACT_GENERATION:-1}"
bundle_version="${OPERATIONAL_BUNDLE_VERSION:-1.4.0}"

fail() {
  echo "operational-store-bootstrap: $*" >&2
  exit 1
}

[[ "$database_name" == "operations" ]] || fail "database identity must be operations"
[[ "$binding_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || fail "invalid binding ID"
[[ "$scope_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || fail "invalid scope ID"
[[ "$host_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || fail "invalid Host ID"
[[ "$scope_id" == "$host_id" ]] || fail "DEV_DEDICATED scope ID must equal Host ID"
[[ "$environment_name" =~ ^[a-z][a-z0-9_-]{0,63}$ ]] || fail "invalid environment"
[[ "$deployment_profile" == "DEV_DEDICATED" ]] || fail "unsupported deployment profile"
[[ "$contract_generation" =~ ^[1-9][0-9]*$ ]] || fail "invalid contract generation"
[[ "$bundle_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "invalid bundle version"
[[ -f "$secret_file" && ! -L "$secret_file" ]] || fail "operational database URL file is missing or unsafe"
[[ -f "$execution_secret_file" && ! -L "$execution_secret_file" ]] || fail "execution database URL file is missing or unsafe"
[[ -f "$workflow_secret_file" && ! -L "$workflow_secret_file" ]] || fail "Workflow database URL file is missing or unsafe"
[[ -f "$a2a_secret_file" && ! -L "$a2a_secret_file" ]] || fail "A2A database URL file is missing or unsafe"
[[ -f "$gateway_secret_file" && ! -L "$gateway_secret_file" ]] || fail "Gateway database URL file is missing or unsafe"
[[ -f "$audit_secret_file" && ! -L "$audit_secret_file" ]] || fail "audit database URL file is missing or unsafe"
[[ -f "$artifact_secret_file" && ! -L "$artifact_secret_file" ]] || fail "artifact database URL file is missing or unsafe"

secret_mode="$(stat -c '%a' "$secret_file")"
secret_mode_value=$((8#$secret_mode))
(( (secret_mode_value & 0037) == 0 )) || fail "operational database URL file permissions are too broad"
database_url="$(<"$secret_file")"
if [[ "$database_url" =~ ^postgres://operations_agent_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  runtime_password="${BASH_REMATCH[1]}"
  secret_host="${BASH_REMATCH[2]}"
else
  fail "operational database URL file has an invalid redacted contract"
fi
[[ "$secret_host" == "${PGHOST:-postgres}" ]] || fail "operational database URL host does not match bootstrap target"
unset database_url

execution_secret_mode="$(stat -c '%a' "$execution_secret_file")"
execution_secret_mode_value=$((8#$execution_secret_mode))
(( (execution_secret_mode_value & 0037) == 0 )) || fail "execution database URL file permissions are too broad"
execution_database_url="$(<"$execution_secret_file")"
if [[ "$execution_database_url" =~ ^postgres://operations_execution_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  execution_runtime_password="${BASH_REMATCH[1]}"
  execution_secret_host="${BASH_REMATCH[2]}"
else
  fail "execution database URL file has an invalid redacted contract"
fi
[[ "$execution_secret_host" == "${PGHOST:-postgres}" ]] || fail "execution database URL host does not match bootstrap target"
unset execution_database_url

workflow_secret_mode="$(stat -c '%a' "$workflow_secret_file")"
workflow_secret_mode_value=$((8#$workflow_secret_mode))
(( (workflow_secret_mode_value & 0037) == 0 )) || fail "Workflow database URL file permissions are too broad"
workflow_database_url="$(<"$workflow_secret_file")"
if [[ "$workflow_database_url" =~ ^postgres://operations_workflow_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  workflow_runtime_password="${BASH_REMATCH[1]}"
  workflow_secret_host="${BASH_REMATCH[2]}"
else
  fail "Workflow database URL file has an invalid redacted contract"
fi
[[ "$workflow_secret_host" == "${PGHOST:-postgres}" ]] || fail "Workflow database URL host does not match bootstrap target"
unset workflow_database_url

a2a_secret_mode="$(stat -c '%a' "$a2a_secret_file")"
a2a_secret_mode_value=$((8#$a2a_secret_mode))
(( (a2a_secret_mode_value & 0037) == 0 )) || fail "A2A database URL file permissions are too broad"
a2a_database_url="$(<"$a2a_secret_file")"
if [[ "$a2a_database_url" =~ ^postgres://operations_a2a_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  a2a_runtime_password="${BASH_REMATCH[1]}"
  a2a_secret_host="${BASH_REMATCH[2]}"
else
  fail "A2A database URL file has an invalid redacted contract"
fi
[[ "$a2a_secret_host" == "${PGHOST:-postgres}" ]] || fail "A2A database URL host does not match bootstrap target"
unset a2a_database_url

gateway_secret_mode="$(stat -c '%a' "$gateway_secret_file")"
gateway_secret_mode_value=$((8#$gateway_secret_mode))
(( (gateway_secret_mode_value & 0037) == 0 )) || fail "Gateway database URL file permissions are too broad"
gateway_database_url="$(<"$gateway_secret_file")"
if [[ "$gateway_database_url" =~ ^postgres://operations_gateway_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  gateway_runtime_password="${BASH_REMATCH[1]}"
  gateway_secret_host="${BASH_REMATCH[2]}"
else
  fail "Gateway database URL file has an invalid redacted contract"
fi
[[ "$gateway_secret_host" == "${PGHOST:-postgres}" ]] || fail "Gateway database URL host does not match bootstrap target"
unset gateway_database_url

audit_secret_mode="$(stat -c '%a' "$audit_secret_file")"
audit_secret_mode_value=$((8#$audit_secret_mode))
(( (audit_secret_mode_value & 0037) == 0 )) || fail "audit database URL file permissions are too broad"
audit_database_url="$(<"$audit_secret_file")"
if [[ "$audit_database_url" =~ ^postgres://operations_audit_publisher:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  audit_publisher_password="${BASH_REMATCH[1]}"
  audit_secret_host="${BASH_REMATCH[2]}"
else
  fail "audit database URL file has an invalid redacted contract"
fi
[[ "$audit_secret_host" == "${PGHOST:-postgres}" ]] || fail "audit database URL host does not match bootstrap target"
unset audit_database_url

artifact_secret_mode="$(stat -c '%a' "$artifact_secret_file")"
artifact_secret_mode_value=$((8#$artifact_secret_mode))
(( (artifact_secret_mode_value & 0037) == 0 )) || fail "artifact database URL file permissions are too broad"
artifact_database_url="$(<"$artifact_secret_file")"
if [[ "$artifact_database_url" =~ ^postgres://operations_artifact_runtime:([0-9a-f]{64})@([a-zA-Z0-9._-]+):([0-9]{1,5})/operations$ ]]; then
  artifact_runtime_password="${BASH_REMATCH[1]}"
  artifact_secret_host="${BASH_REMATCH[2]}"
else
  fail "artifact database URL file has an invalid redacted contract"
fi
[[ "$artifact_secret_host" == "${PGHOST:-postgres}" ]] || fail "artifact database URL host does not match bootstrap target"
unset artifact_database_url

canonical_binding="$binding_id|$host_id|$environment_name|$deployment_profile|$database_name"
computed_binding_digest="sha256:$(printf '%s' "$canonical_binding" | sha256sum | awk '{print $1}')"
[[ "$binding_digest" == "$computed_binding_digest" ]] || fail "binding digest mismatch"

for required_file in bundle.sha256 migration-order.tsv manifest.json; do
  [[ -f "$bundle_root/$required_file" ]] || fail "bundle asset is missing: $required_file"
done
if ! (cd "$bundle_root" && sha256sum -c bundle.sha256 >/dev/null); then
  fail "bundle checksum verification failed"
fi

database_exists="$(psql -U "$database_user" -d postgres -X -tAc \
  "SELECT 1 FROM pg_database WHERE datname = 'operations'")"
if [[ "$database_exists" != "1" ]]; then
  createdb -U "$database_user" "$database_name"
fi

migration_count=0
while IFS=$'\t' read -r order migration_owner schema_name migration_id migration_path migration_sha256; do
  [[ -n "$order" && "$order" != \#* ]] || continue
  [[ "$order" =~ ^[1-9][0-9]*$ ]] || fail "invalid migration order"
  [[ "$migration_owner" =~ ^[a-z][a-z0-9-]*$ ]] || fail "invalid migration owner"
  [[ "$schema_name" =~ ^[a-z][a-z0-9_]*$ ]] || fail "invalid migration schema"
  [[ "$migration_id" =~ ^[0-9]{4}_[a-z0-9_]+$ ]] || fail "invalid migration ID"
  [[ "$migration_path" =~ ^crates/[a-z0-9-]+/migrations/[a-z0-9-]+-postgres/[0-9]{4}_[a-z0-9_]+\.sql$ ]] || fail "invalid migration path"
  [[ "$migration_sha256" =~ ^[0-9a-f]{64}$ ]] || fail "invalid migration checksum"
  [[ -f "$bundle_root/$migration_path" ]] || fail "migration file is missing"
  actual_sha256="$(sha256sum "$bundle_root/$migration_path" | awk '{print $1}')"
  [[ "$actual_sha256" == "$migration_sha256" ]] || fail "migration checksum mismatch for $migration_id"

  ledger_exists="$(psql -U "$database_user" -d "$database_name" -X -tAc \
    "SELECT to_regclass('operational_meta.operational_schema_migration_t') IS NOT NULL")"
  existing_digest=""
  if [[ "$ledger_exists" == "t" ]]; then
    existing_digest="$(psql -U "$database_user" -d "$database_name" -X -tAc \
      "SELECT migration_digest FROM operational_meta.operational_schema_migration_t WHERE migration_owner = '$migration_owner' AND schema_name = '$schema_name' AND migration_id = '$migration_id'")"
  fi
  if [[ -n "$existing_digest" ]]; then
    [[ "$existing_digest" == "sha256:$migration_sha256" ]] || fail "applied migration checksum drift for $migration_id"
  else
    {
      printf 'BEGIN;\n'
      sed -e '/^BEGIN;$/d' -e '/^COMMIT;$/d' "$bundle_root/$migration_path"
      printf "\nINSERT INTO operational_meta.operational_schema_migration_t (migration_owner, schema_name, migration_id, migration_digest, bundle_version, contract_generation) VALUES ('%s', '%s', '%s', 'sha256:%s', '%s', %s);\n" \
        "$migration_owner" "$schema_name" "$migration_id" "$migration_sha256" "$bundle_version" "$contract_generation"
      printf 'COMMIT;\n'
    } | psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
  fi
  migration_count=$((migration_count + 1))
done <"$bundle_root/migration-order.tsv"
(( migration_count > 0 )) || fail "bundle has no ordered migrations"

active_binding="$(psql -U "$database_user" -d "$database_name" -X -tA -F '|' -c \
  "SELECT binding_id, binding_version, binding_digest, scope_kind, scope_id, host_id, environment, database_identity, deployment_profile, schema_contract_generation, active FROM operational_meta.operational_store_binding_t WHERE active")"
expected_binding="$binding_id|1|$binding_digest|HOST_ENVIRONMENT|$scope_id|$host_id|$environment_name|operations|DEV_DEDICATED|$contract_generation|t"
if [[ -z "$active_binding" ]]; then
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 \
    --set=binding_id="$binding_id" \
    --set=binding_digest="$binding_digest" \
    --set=scope_id="$scope_id" \
    --set=host_id="$host_id" \
    --set=environment_name="$environment_name" \
    --set=contract_generation="$contract_generation" <<'SQL' >/dev/null
INSERT INTO operational_meta.operational_store_binding_t (
    binding_id, binding_version, binding_digest, scope_kind, scope_id, host_id,
    environment, database_identity, deployment_profile,
    schema_contract_generation, activated_ts, active
) VALUES (
    :'binding_id'::uuid, 1, :'binding_digest', 'HOST_ENVIRONMENT',
    :'scope_id'::uuid, :'host_id'::uuid, :'environment_name', 'operations',
    'DEV_DEDICATED', :'contract_generation'::bigint, CURRENT_TIMESTAMP, TRUE
);
SQL
elif [[ "$active_binding" != "$expected_binding" ]]; then
  fail "existing active scope root does not match the requested binding"
fi

printf "ALTER ROLE operations_agent_runtime LOGIN PASSWORD '%s';\n" "$runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset runtime_password
printf "ALTER ROLE operations_execution_runtime LOGIN PASSWORD '%s';\n" "$execution_runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset execution_runtime_password
printf "ALTER ROLE operations_workflow_runtime LOGIN PASSWORD '%s';\n" "$workflow_runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset workflow_runtime_password
printf "ALTER ROLE operations_a2a_runtime LOGIN PASSWORD '%s';\n" "$a2a_runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset a2a_runtime_password
printf "ALTER ROLE operations_gateway_runtime LOGIN PASSWORD '%s';\n" "$gateway_runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset gateway_runtime_password
printf "ALTER ROLE operations_audit_publisher LOGIN PASSWORD '%s';\n" "$audit_publisher_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset audit_publisher_password
printf "ALTER ROLE operations_artifact_runtime LOGIN PASSWORD '%s';\n" "$artifact_runtime_password" |
  psql -U "$database_user" -d "$database_name" -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
unset artifact_runtime_password

echo "Operational store bootstrap completed for Host $host_id, environment $environment_name (secret redacted)."
