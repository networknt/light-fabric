#!/usr/bin/env bash
set -euo pipefail

binding_id="${OPERATIONAL_BINDING_ID:?OPERATIONAL_BINDING_ID is required}"
desired_generation="${OPERATIONAL_DESIRED_GENERATION:?OPERATIONAL_DESIRED_GENERATION is required}"
secret_root="${OPERATIONAL_PROVISIONING_SECRET_ROOT:?OPERATIONAL_PROVISIONING_SECRET_ROOT is required}"
overlap_seconds="${OPERATIONAL_CREDENTIAL_OVERLAP_SECONDS:-86400}"
script_root="${OPERATIONAL_SCRIPT_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)}"
binding_suffix="${binding_id//-/}"
container_name="lightapi-ops-${binding_suffix: -12}"
secret_dir="$secret_root/$binding_id"
temporary_dir="$(mktemp -d "$secret_root/.rotation-${binding_id}.XXXXXX")"
trap 'rm -rf -- "$temporary_dir"' EXIT

fail() { echo "rotation: $*" >&2; exit 1; }
for command in date docker; do command -v "$command" >/dev/null || fail "$command is required"; done
[[ "$desired_generation" =~ ^[1-9][0-9]{0,8}$ ]] || fail "invalid desired generation"
[[ "$overlap_seconds" =~ ^[1-9][0-9]*$ ]] || fail "invalid credential overlap"
(( overlap_seconds >= 60 && overlap_seconds <= 604800 )) || fail "credential overlap must be between 60 seconds and 7 days"
[[ -d "$secret_dir" ]] || fail "binding secret directory is absent"

docker container inspect "$container_name" >/dev/null 2>&1 || fail "binding container is absent"
[[ "$(docker inspect --format '{{ index .Config.Labels "net.lightapi.operational-binding-id" }}' "$container_name")" == "$binding_id" ]] ||
  fail "container ownership mismatch"

OPERATIONAL_SECRET_DIR="$temporary_dir" OPERATIONAL_DATABASE_HOST="$container_name" \
  "$script_root/prepare-operational-secret.sh" >/dev/null
if [[ -s "$secret_dir/a2a-authorized-context-key" ]]; then
  install -m 400 "$secret_dir/a2a-authorized-context-key" "$temporary_dir/a2a-authorized-context-key"
fi

declare -A password_files=(
  [operations_agent_runtime]=.operations-agent-runtime-password
  [operations_execution_runtime]=.operations-execution-runtime-password
  [operations_workflow_runtime]=.operations-workflow-runtime-password
  [operations_a2a_runtime]=.operations-a2a-runtime-password
  [operations_gateway_runtime]=.operations-gateway-runtime-password
  [operations_audit_publisher]=.operations-audit-publisher-password
  [operations_artifact_runtime]=.operations-artifact-runtime-password
)
declare -A url_files=(
  [operations_agent_runtime]=operational-database-url
  [operations_execution_runtime]=execution-database-url
  [operations_workflow_runtime]=workflow-database-url
  [operations_a2a_runtime]=a2a-database-url
  [operations_gateway_runtime]=gateway-database-url
  [operations_audit_publisher]=audit-database-url
  [operations_artifact_runtime]=artifact-database-url
)
declare -A search_paths=(
  [operations_agent_runtime]=agent_ops
  [operations_execution_runtime]=execution_ops
  [operations_workflow_runtime]=workflow_ops
  [operations_a2a_runtime]=a2a_ops
  [operations_gateway_runtime]=gateway_ops
  [operations_audit_publisher]=audit_ops
  [operations_artifact_runtime]=artifact_ops
)

expires_at="$(date -u -d "+${overlap_seconds} seconds" '+%Y-%m-%d %H:%M:%S+00')"
for base_role in "${!password_files[@]}"; do
  password="$(<"$temporary_dir/${password_files[$base_role]}")"
  [[ "$password" =~ ^[0-9a-f]{64}$ ]] || fail "generated credential is invalid"
  current_url="$(<"$secret_dir/${url_files[$base_role]}")"
  if [[ "$current_url" =~ ^postgres://${base_role}(_g[1-9][0-9]{0,8})?:[0-9a-f]{64}@${container_name}:5432/operations$ ]]; then
    current_role="${current_url#postgres://}"
    current_role="${current_role%%:*}"
  else
    fail "current ${base_role} URL does not match the binding"
  fi
  new_role="${base_role}_g${desired_generation}"
  role_exists="$(docker exec "$container_name" psql -U postgres -d operations -X -qAt \
    -c "SELECT 1 FROM pg_roles WHERE rolname='$new_role'")"
  if [[ "$role_exists" != "1" ]]; then
    printf 'CREATE ROLE "%s" LOGIN;\n' "$new_role" |
      docker exec -i "$container_name" psql -U postgres -d operations -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
  fi
  printf 'ALTER ROLE "%s" LOGIN PASSWORD '\''%s'\'' VALID UNTIL '\''infinity'\'';\nGRANT "%s" TO "%s";\nALTER ROLE "%s" IN DATABASE operations SET search_path TO %s, operational_meta, public;\n' \
    "$new_role" "$password" "$base_role" "$new_role" "$new_role" "${search_paths[$base_role]}" |
    docker exec -i "$container_name" psql -U postgres -d operations -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
  if [[ "$current_role" != "$new_role" ]]; then
    printf 'ALTER ROLE "%s" VALID UNTIL '\''%s'\'';\n' "$current_role" "$expires_at" |
      docker exec -i "$container_name" psql -U postgres -d operations -X --quiet --set=ON_ERROR_STOP=1 >/dev/null
  fi
  chmod 600 "$temporary_dir/${url_files[$base_role]}"
  printf 'postgres://%s:%s@%s:5432/operations' "$new_role" "$password" "$container_name" > \
    "$temporary_dir/${url_files[$base_role]}"
  unset password current_url
done

printf '%s\n' "$desired_generation" >"$temporary_dir/.credential-generation"
umask 077
previous_dir="$secret_dir/.previous/g${desired_generation}"
mkdir -p -- "$previous_dir"
find "$secret_dir" -maxdepth 1 -type f ! -name '.postgres-admin-password' -exec cp -f -- {} "$previous_dir/" \;
for file in "$temporary_dir"/* "$temporary_dir"/.*; do
  [[ -f "$file" ]] || continue
  mv -f -- "$file" "$secret_dir/$(basename "$file")"
done
find "$secret_dir" -maxdepth 3 -type f -exec chmod 400 {} \;
echo "Rotated runtime credentials for binding $binding_id to generation $desired_generation with ${overlap_seconds}s overlap (values redacted)."
