#!/usr/bin/env bash
set -euo pipefail

if [[ "${DEBUG:-false}" == "true" ]]; then
  set -x
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DEFAULT_RELEASE_BINARY="${WORKSPACE_DIR}/target/release/light-workflow"
DEFAULT_DEBUG_BINARY="${WORKSPACE_DIR}/target/debug/light-workflow"
DEFAULT_ENV_FILE="${LIGHT_WORKFLOW_ENV_FILE:-${SCRIPT_DIR}/light-workflow.env}"

BINARY_PATH="${LIGHT_WORKFLOW_BINARY:-}"
ENV_FILE=""
PREFER_DEBUG=false

show_help() {
  cat <<'EOF'
Usage: ./run.sh [--binary PATH] [--env-file PATH] [--debug-binary]

Starts light-workflow from a compiled native binary. The script keeps the app
working directory at apps/light-workflow for local files and future config.

Required environment:
  DATABASE_URL=postgres://postgres:secret@localhost:5432/configserver

Common optional environment:
  LIGHT_WORKFLOW_HTTP_ADDR=0.0.0.0:8436
  RUST_LOG=light_workflow=debug,info
  WORKFLOW_LOG_ANSI=false

Multi-line shell assignments must be exported or kept on the same command with
line continuations. For repeated runs, prefer ./light-workflow.env.

An env file can be passed with --env-file. If no env file is passed, the script
loads ./light-workflow.env when it exists.
EOF
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

load_env_file() {
  local path="$1"

  [[ -f "$path" ]] || fail "env file not found: $path"
  set -a
  # shellcheck source=/dev/null
  source "$path"
  set +a
}

absolute_path() {
  local path="$1"
  local dir

  dir="$(cd "$(dirname "$path")" && pwd)"
  printf '%s/%s\n' "$dir" "$(basename "$path")"
}

export_if_set() {
  local name="$1"
  local value="$2"

  if [[ -n "$value" ]]; then
    export "${name}=${value}"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      [[ $# -ge 2 ]] || fail "--binary requires a path"
      BINARY_PATH="$2"
      shift 2
      ;;
    --env-file)
      [[ $# -ge 2 ]] || fail "--env-file requires a path"
      ENV_FILE="$2"
      shift 2
      ;;
    --debug-binary)
      PREFER_DEBUG=true
      shift
      ;;
    -h|--help)
      show_help
      exit 0
      ;;
    *)
      show_help >&2
      fail "invalid option: $1"
      ;;
  esac
done

if [[ -n "$ENV_FILE" ]]; then
  load_env_file "$ENV_FILE"
elif [[ -f "$DEFAULT_ENV_FILE" ]]; then
  load_env_file "$DEFAULT_ENV_FILE"
fi

if [[ -z "$BINARY_PATH" && -n "${LIGHT_WORKFLOW_BINARY:-}" ]]; then
  BINARY_PATH="$LIGHT_WORKFLOW_BINARY"
fi

if [[ -z "$BINARY_PATH" ]]; then
  if [[ "$PREFER_DEBUG" == "true" ]]; then
    BINARY_PATH="$DEFAULT_DEBUG_BINARY"
  else
    BINARY_PATH="$DEFAULT_RELEASE_BINARY"
    if [[ ! -x "$BINARY_PATH" && -x "$DEFAULT_DEBUG_BINARY" ]]; then
      BINARY_PATH="$DEFAULT_DEBUG_BINARY"
    fi
  fi
fi

[[ -x "$BINARY_PATH" ]] || fail "binary is not executable: $BINARY_PATH. Build it with: cargo build --release -p light-workflow"
BINARY_PATH="$(absolute_path "$BINARY_PATH")"

export DATABASE_URL="${DATABASE_URL:-${LIGHT_WORKFLOW_DATABASE_URL:-${WORKFLOW_DATABASE_URL:-}}}"
if [[ -z "$DATABASE_URL" ]]; then
  fail "DATABASE_URL is required. Put it in light-workflow.env, export it, or keep DATABASE_URL=... on the same command line as ./run.sh."
fi

export_if_set LIGHT_WORKFLOW_HTTP_ADDR "${LIGHT_WORKFLOW_HTTP_ADDR:-${WORKFLOW_HTTP_ADDR:-}}"
export_if_set RUST_LOG "${RUST_LOG:-}"
export_if_set WORKFLOW_LOG_ANSI "${WORKFLOW_LOG_ANSI:-}"

echo "Starting light-workflow"
echo "  binary: ${BINARY_PATH}"
echo "  database: ${DATABASE_URL}"
echo "  http addr: ${LIGHT_WORKFLOW_HTTP_ADDR:-0.0.0.0:8436}"
echo "  rust log: ${RUST_LOG:-light_workflow=debug,info}"

cd "$SCRIPT_DIR"
exec "$BINARY_PATH"
