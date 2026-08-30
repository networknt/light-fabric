#!/usr/bin/env bash
set -euo pipefail

secret_dir="${OPERATIONAL_SECRET_DIR:-postgres-db/secrets}"
password_file="$secret_dir/.operations-agent-runtime-password"
database_url_file="$secret_dir/operational-database-url"
execution_password_file="$secret_dir/.operations-execution-runtime-password"
execution_database_url_file="$secret_dir/execution-database-url"
workflow_password_file="$secret_dir/.operations-workflow-runtime-password"
workflow_database_url_file="$secret_dir/workflow-database-url"
a2a_password_file="$secret_dir/.operations-a2a-runtime-password"
a2a_database_url_file="$secret_dir/a2a-database-url"
a2a_authorized_context_key_file="$secret_dir/a2a-authorized-context-key"
gateway_password_file="$secret_dir/.operations-gateway-runtime-password"
gateway_database_url_file="$secret_dir/gateway-database-url"
audit_password_file="$secret_dir/.operations-audit-publisher-password"
audit_database_url_file="$secret_dir/audit-database-url"
artifact_password_file="$secret_dir/.operations-artifact-runtime-password"
artifact_database_url_file="$secret_dir/artifact-database-url"
database_host="${OPERATIONAL_DATABASE_HOST:-postgres}"
database_port="${OPERATIONAL_DATABASE_PORT:-5432}"

if [[ ! "$database_host" =~ ^[a-zA-Z0-9._-]+$ ]]; then
  echo "prepare-operational-secret: invalid database host" >&2
  exit 2
fi
if [[ ! "$database_port" =~ ^[0-9]{1,5}$ ]]; then
  echo "prepare-operational-secret: invalid database port" >&2
  exit 2
fi
command -v openssl >/dev/null 2>&1 || {
  echo "prepare-operational-secret: openssl is required" >&2
  exit 2
}

umask 077
mkdir -p -- "$secret_dir"
chmod 700 "$secret_dir"

if [[ -d "$password_file" || -d "$database_url_file" || -d "$execution_password_file" || -d "$execution_database_url_file" || -d "$workflow_password_file" || -d "$workflow_database_url_file" || -d "$a2a_password_file" || -d "$a2a_database_url_file" || -d "$a2a_authorized_context_key_file" || -d "$gateway_password_file" || -d "$gateway_database_url_file" || -d "$audit_password_file" || -d "$audit_database_url_file" || -d "$artifact_password_file" || -d "$artifact_database_url_file" ]]; then
  echo "prepare-operational-secret: a secret path is a directory" >&2
  exit 1
fi
trap 'rm -f -- "${temporary_password:-}" "${temporary_url:-}" "${temporary_execution_password:-}" "${temporary_execution_url:-}" "${temporary_workflow_password:-}" "${temporary_workflow_url:-}" "${temporary_a2a_password:-}" "${temporary_a2a_url:-}" "${temporary_a2a_context_key:-}" "${temporary_gateway_password:-}" "${temporary_gateway_url:-}" "${temporary_audit_password:-}" "${temporary_audit_url:-}" "${temporary_artifact_password:-}" "${temporary_artifact_url:-}"' EXIT

if [[ -s "$password_file" ]]; then
  password="$(<"$password_file")"
  [[ "$password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_password="$(mktemp "$secret_dir/.operations-password.XXXXXX")"
  openssl rand -hex 32 >"$temporary_password"
  tr -d '\n' <"$temporary_password" >"$temporary_password.compact"
  mv -- "$temporary_password.compact" "$password_file"
  rm -f -- "$temporary_password"
  password="$(<"$password_file")"
fi

expected_url="postgres://operations_agent_runtime:${password}@${database_host}:${database_port}/operations"
if [[ -s "$database_url_file" ]]; then
  [[ "$(<"$database_url_file")" == "$expected_url" ]] || {
    echo "prepare-operational-secret: existing operational database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_url="$(mktemp "$secret_dir/.operations-url.XXXXXX")"
  printf '%s' "$expected_url" >"$temporary_url"
  mv -- "$temporary_url" "$database_url_file"
fi

if [[ -s "$execution_password_file" ]]; then
  execution_password="$(<"$execution_password_file")"
  [[ "$execution_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored execution runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_execution_password="$(mktemp "$secret_dir/.operations-execution-password.XXXXXX")"
  openssl rand -hex 32 >"$temporary_execution_password"
  tr -d '\n' <"$temporary_execution_password" >"$temporary_execution_password.compact"
  mv -- "$temporary_execution_password.compact" "$execution_password_file"
  rm -f -- "$temporary_execution_password"
  execution_password="$(<"$execution_password_file")"
fi

expected_execution_url="postgres://operations_execution_runtime:${execution_password}@${database_host}:${database_port}/operations"
if [[ -s "$execution_database_url_file" ]]; then
  [[ "$(<"$execution_database_url_file")" == "$expected_execution_url" ]] || {
    echo "prepare-operational-secret: existing execution database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_execution_url="$(mktemp "$secret_dir/.operations-execution-url.XXXXXX")"
  printf '%s' "$expected_execution_url" >"$temporary_execution_url"
  mv -- "$temporary_execution_url" "$execution_database_url_file"
fi

if [[ -s "$workflow_password_file" ]]; then
  workflow_password="$(<"$workflow_password_file")"
  [[ "$workflow_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored Workflow runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_workflow_password="$(mktemp "$secret_dir/.operations-workflow-password.XXXXXX")"
  openssl rand -hex 32 >"$temporary_workflow_password"
  tr -d '\n' <"$temporary_workflow_password" >"$temporary_workflow_password.compact"
  mv -- "$temporary_workflow_password.compact" "$workflow_password_file"
  rm -f -- "$temporary_workflow_password"
  workflow_password="$(<"$workflow_password_file")"
fi

expected_workflow_url="postgres://operations_workflow_runtime:${workflow_password}@${database_host}:${database_port}/operations"
if [[ -s "$workflow_database_url_file" ]]; then
  [[ "$(<"$workflow_database_url_file")" == "$expected_workflow_url" ]] || {
    echo "prepare-operational-secret: existing Workflow database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_workflow_url="$(mktemp "$secret_dir/.operations-workflow-url.XXXXXX")"
  printf '%s' "$expected_workflow_url" >"$temporary_workflow_url"
  mv -- "$temporary_workflow_url" "$workflow_database_url_file"
fi

if [[ -s "$a2a_password_file" ]]; then
  a2a_password="$(<"$a2a_password_file")"
  [[ "$a2a_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored A2A runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_a2a_password="$(mktemp "$secret_dir/.operations-a2a-password.XXXXXX")"
  openssl rand -hex 32 >"$temporary_a2a_password"
  tr -d '\n' <"$temporary_a2a_password" >"$temporary_a2a_password.compact"
  mv -- "$temporary_a2a_password.compact" "$a2a_password_file"
  rm -f -- "$temporary_a2a_password"
  a2a_password="$(<"$a2a_password_file")"
fi

expected_a2a_url="postgres://operations_a2a_runtime:${a2a_password}@${database_host}:${database_port}/operations"
if [[ -s "$a2a_database_url_file" ]]; then
  [[ "$(<"$a2a_database_url_file")" == "$expected_a2a_url" ]] || {
    echo "prepare-operational-secret: existing A2A database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_a2a_url="$(mktemp "$secret_dir/.operations-a2a-url.XXXXXX")"
  printf '%s' "$expected_a2a_url" >"$temporary_a2a_url"
  mv -- "$temporary_a2a_url" "$a2a_database_url_file"
fi

if [[ -s "$a2a_authorized_context_key_file" ]]; then
  a2a_context_key="$(<"$a2a_authorized_context_key_file")"
  [[ "$a2a_context_key" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored A2A authorized-context key is invalid" >&2
    exit 1
  }
else
  temporary_a2a_context_key="$(mktemp "$secret_dir/.a2a-authorized-context-key.XXXXXX")"
  openssl rand -hex 32 | tr -d '\n' >"$temporary_a2a_context_key"
  mv -- "$temporary_a2a_context_key" "$a2a_authorized_context_key_file"
fi

if [[ -s "$gateway_password_file" ]]; then
  gateway_password="$(<"$gateway_password_file")"
  [[ "$gateway_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored Gateway runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_gateway_password="$(mktemp "$secret_dir/.operations-gateway-password.XXXXXX")"
  openssl rand -hex 32 | tr -d '\n' >"$temporary_gateway_password"
  mv -- "$temporary_gateway_password" "$gateway_password_file"
  gateway_password="$(<"$gateway_password_file")"
fi

expected_gateway_url="postgres://operations_gateway_runtime:${gateway_password}@${database_host}:${database_port}/operations"
if [[ -s "$gateway_database_url_file" ]]; then
  [[ "$(<"$gateway_database_url_file")" == "$expected_gateway_url" ]] || {
    echo "prepare-operational-secret: existing Gateway database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_gateway_url="$(mktemp "$secret_dir/.operations-gateway-url.XXXXXX")"
  printf '%s' "$expected_gateway_url" >"$temporary_gateway_url"
  mv -- "$temporary_gateway_url" "$gateway_database_url_file"
fi

if [[ -s "$audit_password_file" ]]; then
  audit_password="$(<"$audit_password_file")"
  [[ "$audit_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored audit publisher credential is invalid" >&2
    exit 1
  }
else
  temporary_audit_password="$(mktemp "$secret_dir/.operations-audit-password.XXXXXX")"
  openssl rand -hex 32 | tr -d '\n' >"$temporary_audit_password"
  mv -- "$temporary_audit_password" "$audit_password_file"
  audit_password="$(<"$audit_password_file")"
fi

expected_audit_url="postgres://operations_audit_publisher:${audit_password}@${database_host}:${database_port}/operations"
if [[ -s "$audit_database_url_file" ]]; then
  [[ "$(<"$audit_database_url_file")" == "$expected_audit_url" ]] || {
    echo "prepare-operational-secret: existing audit database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_audit_url="$(mktemp "$secret_dir/.operations-audit-url.XXXXXX")"
  printf '%s' "$expected_audit_url" >"$temporary_audit_url"
  mv -- "$temporary_audit_url" "$audit_database_url_file"
fi

if [[ -s "$artifact_password_file" ]]; then
  artifact_password="$(<"$artifact_password_file")"
  [[ "$artifact_password" =~ ^[0-9a-f]{64}$ ]] || {
    echo "prepare-operational-secret: stored artifact runtime credential is invalid" >&2
    exit 1
  }
else
  temporary_artifact_password="$(mktemp "$secret_dir/.operations-artifact-password.XXXXXX")"
  openssl rand -hex 32 | tr -d '\n' >"$temporary_artifact_password"
  mv -- "$temporary_artifact_password" "$artifact_password_file"
  artifact_password="$(<"$artifact_password_file")"
fi

expected_artifact_url="postgres://operations_artifact_runtime:${artifact_password}@${database_host}:${database_port}/operations"
if [[ -s "$artifact_database_url_file" ]]; then
  [[ "$(<"$artifact_database_url_file")" == "$expected_artifact_url" ]] || {
    echo "prepare-operational-secret: existing artifact database URL does not match its credential" >&2
    exit 1
  }
else
  temporary_artifact_url="$(mktemp "$secret_dir/.operations-artifact-url.XXXXXX")"
  printf '%s' "$expected_artifact_url" >"$temporary_artifact_url"
  mv -- "$temporary_artifact_url" "$artifact_database_url_file"
fi

chmod 400 "$password_file" "$database_url_file" "$execution_password_file" "$execution_database_url_file" \
  "$workflow_password_file" "$workflow_database_url_file" "$a2a_password_file" "$a2a_database_url_file" \
  "$a2a_authorized_context_key_file" "$gateway_password_file" "$gateway_database_url_file" \
  "$audit_password_file" "$audit_database_url_file" "$artifact_password_file" "$artifact_database_url_file"
echo "Prepared Agent, execution, Workflow, A2A, Gateway, audit, and artifact database URL files in $secret_dir (contents redacted)."
