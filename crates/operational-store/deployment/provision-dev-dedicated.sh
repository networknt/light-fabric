#!/usr/bin/env bash
set -euo pipefail

binding_id="${OPERATIONAL_BINDING_ID:?OPERATIONAL_BINDING_ID is required}"
host_id="${OPERATIONAL_HOST_ID:?OPERATIONAL_HOST_ID is required}"
environment_name="${OPERATIONAL_ENVIRONMENT:?OPERATIONAL_ENVIRONMENT is required}"
binding_digest="${OPERATIONAL_BINDING_DIGEST:?OPERATIONAL_BINDING_DIGEST is required}"
generation="${OPERATIONAL_DESIRED_GENERATION:?OPERATIONAL_DESIRED_GENERATION is required}"
network_name="${OPERATIONAL_DOCKER_NETWORK:?OPERATIONAL_DOCKER_NETWORK is required}"
secret_root="${OPERATIONAL_PROVISIONING_SECRET_ROOT:?OPERATIONAL_PROVISIONING_SECRET_ROOT is required}"
bundle_root="${OPERATIONAL_BUNDLE_ROOT:?OPERATIONAL_BUNDLE_ROOT is required}"
script_root="${OPERATIONAL_SCRIPT_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)}"
image="${OPERATIONAL_POSTGRES_IMAGE:-pgvector/pgvector:pg17}"
binding_suffix="${binding_id//-/}"
container_name="lightapi-ops-${binding_suffix: -12}"
secret_dir="$secret_root/$binding_id"
admin_password_file="$secret_dir/.postgres-admin-password"

fail() { echo "dev-dedicated-provider: $*" >&2; exit 1; }
for command in docker openssl; do command -v "$command" >/dev/null || fail "$command is required"; done
[[ "$binding_id" =~ ^[0-9a-f-]{36}$ && "$host_id" =~ ^[0-9a-f-]{36}$ ]] || fail "invalid binding or Host ID"
[[ "$environment_name" =~ ^[a-z][a-z0-9_-]{0,31}$ ]] || fail "invalid environment"
[[ "$binding_digest" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid binding digest"
[[ "$generation" =~ ^[1-9][0-9]*$ ]] || fail "invalid desired generation"
[[ -d "$bundle_root" && -x "$script_root/bootstrap-operational-store.sh" && -x "$script_root/prepare-operational-secret.sh" ]] || fail "canonical bundle or scripts are unavailable"
docker network inspect "$network_name" >/dev/null 2>&1 || fail "Docker network does not exist: $network_name"

umask 077
mkdir -p -- "$secret_dir"
chmod 700 "$secret_dir"
if [[ ! -s "$admin_password_file" ]]; then
  temporary_admin="$(mktemp "$secret_dir/.admin.XXXXXX")"
  openssl rand -hex 32 | tr -d '\n' >"$temporary_admin"
  mv -- "$temporary_admin" "$admin_password_file"
fi
chmod 400 "$admin_password_file"

if docker container inspect "$container_name" >/dev/null 2>&1; then
  actual_binding="$(docker inspect --format '{{ index .Config.Labels "net.lightapi.operational-binding-id" }}' "$container_name")"
  actual_host="$(docker inspect --format '{{ index .Config.Labels "net.lightapi.host-id" }}' "$container_name")"
  actual_environment="$(docker inspect --format '{{ index .Config.Labels "net.lightapi.environment" }}' "$container_name")"
  [[ "$actual_binding" == "$binding_id" && "$actual_host" == "$host_id" && "$actual_environment" == "$environment_name" ]] ||
    fail "existing container ownership labels do not match the binding"
  [[ "$(docker inspect --format '{{.State.Running}}' "$container_name")" == "true" ]] || docker start "$container_name" >/dev/null
else
  docker run -d --name "$container_name" --hostname "$container_name" \
    --network "$network_name" --network-alias "$container_name" \
    --label "net.lightapi.operational-binding-id=$binding_id" \
    --label "net.lightapi.host-id=$host_id" \
    --label "net.lightapi.environment=$environment_name" \
    --label "net.lightapi.desired-generation=$generation" \
    -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD_FILE=/run/secrets/.postgres-admin-password \
    -v "$secret_dir:/run/secrets:ro" \
    -v "$bundle_root:/opt/operational-store/bundle:ro" \
    -v "$script_root:/opt/operational-store/bin:ro" \
    "$image" >/dev/null
fi

for _ in $(seq 1 60); do
  docker exec "$container_name" pg_isready -U postgres -d postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$container_name" pg_isready -U postgres -d postgres >/dev/null 2>&1 || fail "PostgreSQL did not become ready"

OPERATIONAL_SECRET_DIR="$secret_dir" OPERATIONAL_DATABASE_HOST="$container_name" \
  "$script_root/prepare-operational-secret.sh" >/dev/null

admin_password="$(<"$admin_password_file")"
printf '%s\n' "$admin_password" | docker exec -i \
  -e POSTGRES_USER=postgres -e PGHOST="$container_name" \
  -e OPERATIONAL_BINDING_ID="$binding_id" -e OPERATIONAL_BINDING_DIGEST="$binding_digest" \
  -e OPERATIONAL_SCOPE_ID="$host_id" -e OPERATIONAL_HOST_ID="$host_id" \
  -e OPERATIONAL_ENVIRONMENT="$environment_name" -e OPERATIONAL_DEPLOYMENT_PROFILE=DEV_DEDICATED \
  -e OPERATIONAL_CONTRACT_GENERATION=1 -e OPERATIONAL_BUNDLE_VERSION=1.4.0 \
  "$container_name" sh -c 'IFS= read -r PGPASSWORD; export PGPASSWORD; exec /opt/operational-store/bin/bootstrap-operational-store.sh' >/dev/null
unset admin_password

docker exec "$container_name" psql -U postgres -d operations -X -tAc \
  "SELECT 1 FROM operational_meta.operational_store_binding_t WHERE binding_id='$binding_id'::uuid AND host_id='$host_id'::uuid AND environment='$environment_name' AND binding_digest='$binding_digest' AND active" |
  grep -qx 1 || fail "scope-root readiness validation failed"

printf 'docker://%s\n' "$container_name"
