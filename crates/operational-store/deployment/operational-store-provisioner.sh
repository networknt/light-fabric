#!/usr/bin/env bash
set -euo pipefail

control_url_file="${PORTAL_CONTROL_DATABASE_URL_FILE:?PORTAL_CONTROL_DATABASE_URL_FILE is required}"
command_url="${PORTAL_COMMAND_URL:?PORTAL_COMMAND_URL is required}"
token_file="${PORTAL_COMMAND_TOKEN_FILE:?PORTAL_COMMAND_TOKEN_FILE is required}"
command_ca_file="${PORTAL_COMMAND_CA_FILE:-}"
command_connect_to="${PORTAL_COMMAND_CONNECT_TO:-}"
secret_root="${OPERATIONAL_PROVISIONING_SECRET_ROOT:?OPERATIONAL_PROVISIONING_SECRET_ROOT is required}"
worker_id="${OPERATIONAL_PROVISIONER_ID:-$(hostname)-$$}"
poll_seconds="${OPERATIONAL_PROVISIONER_POLL_SECONDS:-5}"
lease_seconds="${OPERATIONAL_PROVISIONER_LEASE_SECONDS:-120}"
run_once="${OPERATIONAL_PROVISIONER_RUN_ONCE:-false}"
script_root="${OPERATIONAL_SCRIPT_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)}"
curl_config_file=""

cleanup() {
  [[ -z "$curl_config_file" ]] || rm -f -- "$curl_config_file"
}
trap cleanup EXIT

fail() { echo "operational-store-provisioner: $*" >&2; exit 1; }
for command in psql curl docker flock; do command -v "$command" >/dev/null || fail "$command is required"; done
for file in "$control_url_file" "$token_file"; do
  [[ -f "$file" && ! -L "$file" ]] || fail "required secret file is missing or unsafe"
  mode_value=$((8#$(stat -c '%a' "$file")))
  (( (mode_value & 0037) == 0 )) || fail "secret file permissions are too broad"
done
if [[ -n "$command_ca_file" ]]; then
  [[ -f "$command_ca_file" && ! -L "$command_ca_file" ]] || fail "Portal command CA file is missing or unsafe"
fi
control_url="$(<"$control_url_file")"
token="$(<"$token_file")"
if [[ "$control_url" =~ ^(postgres|postgresql)://([^:/@]+):([^@/]*)@([^:/?]+):([0-9]+)/([^?]+)(\?options=([^&]+))?$ ]]; then
  printf -v control_user '%b' "${BASH_REMATCH[2]//%/\\x}"
  printf -v control_password '%b' "${BASH_REMATCH[3]//%/\\x}"
  control_host="${BASH_REMATCH[4]}"
  control_port="${BASH_REMATCH[5]}"
  printf -v control_database '%b' "${BASH_REMATCH[6]//%/\\x}"
  printf -v control_options '%b' "${BASH_REMATCH[8]//%/\\x}"
else
  fail "Portal control database URL is invalid"
fi
unset control_url
[[ "$lease_seconds" =~ ^[1-9][0-9]*$ && "$poll_seconds" =~ ^[1-9][0-9]*$ ]] || fail "invalid timing configuration"
umask 077
mkdir -p -- "$secret_root/.locks"
curl_config_file="$(mktemp)"
printf 'url = "%s"\nheader = "Authorization: Bearer %s"\n' "$command_url" "$token" >"$curl_config_file"
[[ -z "$command_ca_file" ]] || printf 'cacert = "%s"\n' "$command_ca_file" >>"$curl_config_file"
[[ -z "$command_connect_to" ]] || printf 'connect-to = "%s"\n' "$command_connect_to" >>"$curl_config_file"
unset token command_url

renew_lease() {
  local job_id="$1" lease_owner="$2" fencing_token="$3"
  [[ "$(PGHOST="$control_host" PGPORT="$control_port" PGDATABASE="$control_database" \
    PGUSER="$control_user" PGPASSWORD="$control_password" PGOPTIONS="$control_options" \
    psql -X -qAt --set=ON_ERROR_STOP=1 \
    --set=job_id="$job_id" --set=lease_owner="$lease_owner" --set=fencing_token="$fencing_token" \
    --set=lease_seconds="$lease_seconds" <<'SQL'
UPDATE operational_store_provisioning_job_t
SET lease_expires_ts=now()+(:'lease_seconds'||' seconds')::interval,update_ts=now()
WHERE job_id=:'job_id'::uuid AND job_state='CLAIMED' AND lease_owner=:'lease_owner'
  AND fencing_token=:'fencing_token'::bigint AND lease_expires_ts>now()
RETURNING 1;
SQL
)" == "1" ]]
}

post_command() {
  local action="$1" host_id="$2" binding_id="$3" job_id="$4" lease_owner="$5" fencing_token="$6"
  local failure_code="${7:-}" provider_ref="${8:-}" payload response
  renew_lease "$job_id" "$lease_owner" "$fencing_token" ||
    fail "worker lease was lost before lifecycle action $action"
  payload="$(printf '{"host":"lightapi.net","service":"host","action":"%s","version":"0.1.0","data":{"targetHostId":"%s","bindingId":"%s","jobId":"%s","leaseOwner":"%s","fencingToken":%s' \
    "$action" "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token")"
  [[ -z "$failure_code" ]] || payload+=",\"failureCode\":\"$failure_code\""
  [[ -z "$provider_ref" ]] || payload+=",\"providerResourceRef\":\"$provider_ref\""
  payload+='}}'
  response="$(curl --config "$curl_config_file" --fail --silent --show-error --max-time 30 \
    -H 'Content-Type: application/json' --data "$payload"
  )"
  if [[ "$response" == *'"statusCode":'* || "$response" == *'"error":'* ]]; then
    fail "Portal rejected lifecycle action $action"
  fi
}

claim_job() {
  PGHOST="$control_host" PGPORT="$control_port" PGDATABASE="$control_database" \
    PGUSER="$control_user" PGPASSWORD="$control_password" PGOPTIONS="$control_options" \
    psql -X -qAt -F '|' --set=ON_ERROR_STOP=1 \
    --set=worker_id="$worker_id" --set=lease_seconds="$lease_seconds" <<'SQL'
WITH candidate AS (
 SELECT j.job_id FROM operational_store_provisioning_job_t j
 JOIN operational_store_binding_t b ON b.binding_id=j.binding_id
 WHERE (j.job_state='PENDING' OR (j.job_state='CLAIMED' AND j.lease_expires_ts<now()))
   AND j.next_attempt_ts<=now()
   AND NOT (j.operation_kind='DECOMMISSION' AND b.retention_hold)
 ORDER BY j.created_ts FOR UPDATE SKIP LOCKED LIMIT 1
), claimed AS (
 UPDATE operational_store_provisioning_job_t j SET job_state='CLAIMED',lease_owner=:'worker_id',
  lease_expires_ts=now()+(:'lease_seconds'||' seconds')::interval,attempt_count=attempt_count+1,
  fencing_token=fencing_token+1,update_ts=now()
 FROM candidate c WHERE j.job_id=c.job_id
 RETURNING j.job_id,j.binding_id,j.desired_generation,j.operation_kind,j.lease_owner,j.fencing_token
)
SELECT c.job_id,c.binding_id,b.host_id,b.environment,c.desired_generation,c.operation_kind,
       b.binding_digest,b.lifecycle_state,c.lease_owner,c.fencing_token
FROM claimed c JOIN operational_store_binding_t b ON b.binding_id=c.binding_id;
SQL
}

process_job() {
  local record="$1" job_id binding_id host_id environment generation operation digest lifecycle_state lease_owner fencing_token
  local provider_ref binding_suffix container lock_file lock_fd
  IFS='|' read -r job_id binding_id host_id environment generation operation digest lifecycle_state lease_owner fencing_token <<<"$record"
  lock_file="$secret_root/.locks/$binding_id.lock"
  exec {lock_fd}>"$lock_file"
  flock "$lock_fd"
  renew_lease "$job_id" "$lease_owner" "$fencing_token" || {
    echo "operational-store-provisioner: stale claim skipped for binding $binding_id" >&2
    flock -u "$lock_fd"
    exec {lock_fd}>&-
    return
  }
  export OPERATIONAL_BINDING_ID="$binding_id" OPERATIONAL_HOST_ID="$host_id" OPERATIONAL_ENVIRONMENT="$environment"
  export OPERATIONAL_DESIRED_GENERATION="$generation" OPERATIONAL_BINDING_DIGEST="$digest" OPERATIONAL_SCRIPT_ROOT="$script_root"
  case "$operation" in
    PROVISION|RETRY)
      if [[ "$lifecycle_state" == "REQUESTED" ]]; then
        post_command startOperationalStoreProvisioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token"
      elif [[ "$lifecycle_state" != "PROVISIONING" ]]; then
        fail "cannot reconcile $operation from lifecycle state $lifecycle_state"
      fi
      if provider_ref="$("$script_root/provision-dev-dedicated.sh")"; then
        post_command completeOperationalStoreProvisioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token" "" "$provider_ref"
      else
        post_command failOperationalStoreProvisioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token" PROVIDER_RECONCILIATION_FAILED
      fi ;;
    ROTATE)
      if "$script_root/rotate-dev-dedicated-credentials.sh"; then
        post_command completeOperationalStoreCredentialRotation "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token"
      else
        post_command failOperationalStoreProvisioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token" CREDENTIAL_ROTATION_FAILED
      fi ;;
    DEACTIVATE)
      binding_suffix="${binding_id//-/}"; container="lightapi-ops-${binding_suffix: -12}"
      if docker stop "$container" >/dev/null; then
        post_command completeOperationalStoreDeactivation "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token"
      else
        post_command failOperationalStoreProvisioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token" DEACTIVATION_FAILED
      fi ;;
    DECOMMISSION)
      if [[ "$lifecycle_state" == "DECOMMISSION_REQUESTED" ]]; then
        post_command startOperationalStoreDecommissioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token"
      elif [[ "$lifecycle_state" != "DECOMMISSIONING" ]]; then
        fail "cannot reconcile DECOMMISSION from lifecycle state $lifecycle_state"
      fi
      binding_suffix="${binding_id//-/}"; container="lightapi-ops-${binding_suffix: -12}"
      docker stop "$container" >/dev/null 2>&1 || true
      post_command completeOperationalStoreDecommissioning "$host_id" "$binding_id" "$job_id" "$lease_owner" "$fencing_token" ;;
    *) fail "unsupported operation $operation" ;;
  esac
  flock -u "$lock_fd"
  exec {lock_fd}>&-
}

while true; do
  record="$(claim_job)"
  if [[ -n "$record" ]]; then process_job "$record"; fi
  [[ "$run_once" == "true" ]] && break
  sleep "$poll_seconds"
done
