#!/usr/bin/env bash
set -euo pipefail

suite="${1:-all}"
test_timeout="${LIGHT_LIVE_TEST_TIMEOUT:-15m}"

require() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "required environment variable is not set: ${name}" >&2
    exit 2
  fi
}

run() {
  echo "+ $*"
  timeout --foreground "${test_timeout}" "$@"
}

run_oci() {
  require LIGHT_OCI_CONFORMANCE_IMAGE
  if [[ "${LIGHT_OCI_CONFORMANCE_IMAGE}" != *@sha256:* ]]; then
    echo "LIGHT_OCI_CONFORMANCE_IMAGE must be pinned by sha256 digest" >&2
    exit 2
  fi
  command -v docker >/dev/null
  docker info >/dev/null
  docker image inspect "${LIGHT_OCI_CONFORMANCE_IMAGE}" >/dev/null 2>&1 ||
    docker pull "${LIGHT_OCI_CONFORMANCE_IMAGE}"
  run cargo test -p execution-backend-oci --test docker_conformance -- --ignored --nocapture --test-threads=1
}

run_cube() {
  require LIGHT_CUBE_TEST_API_URL
  require LIGHT_CUBE_TEST_API_KEY_FILE
  require LIGHT_CUBE_TEST_TEMPLATE_ID
  [[ -r "${LIGHT_CUBE_TEST_API_KEY_FILE}" ]] || {
    echo "LIGHT_CUBE_TEST_API_KEY_FILE is not readable" >&2
    exit 2
  }
  if [[ -n "${LIGHT_CUBE_TEST_TLS_CA_FILE:-}" && ! -r "${LIGHT_CUBE_TEST_TLS_CA_FILE}" ]]; then
    echo "LIGHT_CUBE_TEST_TLS_CA_FILE is not readable" >&2
    exit 2
  fi
  run cargo test -p execution-backend-cube --test cube_failure_matrix_live -- --ignored --nocapture --test-threads=1
  run cargo test -p execution-backend-cube --test cube_coding_live -- --ignored --nocapture --test-threads=1
}

case "${suite}" in
  compile)
    run cargo test -p execution-backend-oci --test docker_conformance --no-run
    run cargo test -p execution-backend-cube --test cube_failure_matrix_live --no-run
    run cargo test -p execution-backend-cube --test cube_coding_live --no-run
    ;;
  oci) run_oci ;;
  cube) run_cube ;;
  all)
    run_oci
    run_cube
    ;;
  *)
    echo "usage: $0 {compile|oci|cube|all}" >&2
    exit 2
    ;;
esac
