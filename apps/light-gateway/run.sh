#!/usr/bin/env bash
set -euo pipefail

if [[ "${DEBUG:-false}" == "true" ]]; then
  set -x
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DEFAULT_RELEASE_BINARY="${WORKSPACE_DIR}/target/release/light-gateway"
DEFAULT_DEBUG_BINARY="${WORKSPACE_DIR}/target/debug/light-gateway"
DEFAULT_ENV_FILE="${LIGHT_GATEWAY_ENV_FILE:-${SCRIPT_DIR}/light-gateway.env}"

BINARY_PATH="${LIGHT_GATEWAY_BINARY:-}"
ENV_FILE=""
PREFER_DEBUG=false

show_help() {
  cat <<'EOF'
Usage: ./run.sh [--binary PATH] [--env-file PATH] [--debug-binary]

Starts light-gateway from a compiled native binary. The script keeps the app
working directory at apps/light-gateway because the runtime loads relative
config and config-cache paths from there.

Required environment:
  LIGHT_PORTAL_AUTHORIZATION=Bearer <token>

Common optional environment:
  LIGHT_CONFIG_SERVER_URI=https://config-server.example.com:8435
  PORTAL_REGISTRY_URL=https://controller.example.com:8438
  SERVER_ADVERTISED_ADDRESS=gateway.example.com
  LIGHT_GATEWAY_SERVICE_ID=com.networknt.light-gateway-1.0.0
  LIGHT_GATEWAY_ENV=dev

An env file can be passed with --env-file. If no env file is passed, the script
loads ./light-gateway.env when it exists.
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

if [[ -z "$BINARY_PATH" && -n "${LIGHT_GATEWAY_BINARY:-}" ]]; then
  BINARY_PATH="$LIGHT_GATEWAY_BINARY"
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

[[ -x "$BINARY_PATH" ]] || fail "binary is not executable: $BINARY_PATH. Build it with: cargo build --release -p light-gateway"
BINARY_PATH="$(absolute_path "$BINARY_PATH")"

[[ -d "${SCRIPT_DIR}/config" ]] || fail "missing config directory: ${SCRIPT_DIR}/config"
mkdir -p "${SCRIPT_DIR}/config-cache"

export LIGHT_PORTAL_AUTHORIZATION="${LIGHT_PORTAL_AUTHORIZATION:-${LIGHT_PORTAL_TOKEN:-}}"
if [[ -z "$LIGHT_PORTAL_AUTHORIZATION" ]]; then
  fail "LIGHT_PORTAL_AUTHORIZATION is required for config-server bootstrap and controller registration. Put it in light-gateway.env or keep the multi-line command contiguous with no blank line before ./run.sh."
fi

export_if_set LIGHT_CONFIG_SERVER_URI "${LIGHT_CONFIG_SERVER_URI:-${CONFIG_SERVER_URI:-}}"
export_if_set PORTALREGISTRY_PORTALURL "${PORTALREGISTRY_PORTALURL:-${PORTAL_REGISTRY_URL:-}}"

export_if_set STARTUP_HOST "${STARTUP_HOST:-${LIGHT_GATEWAY_HOST:-}}"
export_if_set STARTUP_BOOTSTRAPCACERTPATH "${STARTUP_BOOTSTRAPCACERTPATH:-${BOOTSTRAP_CA_CERT_PATH:-}}"

export_if_set SERVER_SERVICEID "${SERVER_SERVICEID:-${LIGHT_GATEWAY_SERVICE_ID:-}}"
export_if_set SERVER_ENVIRONMENT "${SERVER_ENVIRONMENT:-${LIGHT_GATEWAY_ENV:-${LIGHT_ENV:-}}}"
export_if_set SERVER_IP "${SERVER_IP:-}"
export_if_set SERVER_ADVERTISEDADDRESS "${SERVER_ADVERTISEDADDRESS:-${SERVER_ADVERTISED_ADDRESS:-${STATUS_HOST_IP:-}}}"
export_if_set SERVER_HTTPPORT "${SERVER_HTTPPORT:-${SERVER_HTTP_PORT:-}}"
export_if_set SERVER_ENABLEHTTP "${SERVER_ENABLEHTTP:-${SERVER_ENABLE_HTTP:-}}"
export_if_set SERVER_HTTPSPORT "${SERVER_HTTPSPORT:-${SERVER_HTTPS_PORT:-}}"
export_if_set SERVER_ENABLEHTTPS "${SERVER_ENABLEHTTPS:-${SERVER_ENABLE_HTTPS:-}}"
export_if_set SERVER_ENABLEREGISTRY "${SERVER_ENABLEREGISTRY:-${SERVER_ENABLE_REGISTRY:-}}"
export_if_set SERVER_STARTONREGISTRYFAILURE "${SERVER_STARTONREGISTRYFAILURE:-${SERVER_START_ON_REGISTRY_FAILURE:-}}"

export_if_set CLIENT_CACERTPATH "${CLIENT_CACERTPATH:-${CLIENT_CA_CERT_PATH:-}}"
export_if_set CLIENT_VERIFYHOSTNAME "${CLIENT_VERIFYHOSTNAME:-${CLIENT_VERIFY_HOSTNAME:-}}"

echo "Starting light-gateway"
echo "  binary: ${BINARY_PATH}"
echo "  config server: ${LIGHT_CONFIG_SERVER_URI:-from config}"
echo "  controller: ${PORTALREGISTRY_PORTALURL:-from config}"
echo "  service id: ${SERVER_SERVICEID:-from config}"
echo "  advertised address: ${SERVER_ADVERTISEDADDRESS:-from config}"

cd "$SCRIPT_DIR"
exec "$BINARY_PATH"
