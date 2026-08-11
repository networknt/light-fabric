#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEST_ROOT="$(mktemp -d)"

cleanup() {
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

mkdir -p "${TEST_ROOT}/bin"
cat > "${TEST_ROOT}/bin/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${DOCKER_LOG:?}"
if [[ -n "${DOCKER_FAIL_APP:-}" && "$*" == *"networknt/${DOCKER_FAIL_APP}:"* ]]; then
  exit 19
fi
STUB
chmod +x "${TEST_ROOT}/bin/docker"

export PATH="${TEST_ROOT}/bin:${PATH}"
export DOCKER_LOG="${TEST_ROOT}/docker.log"

APPS=(
  "light-agent"
  "light-deployer"
  "light-gateway"
  "light-workflow"
  "light-workflow-runner"
  "light-knowledge"
  "light-knowledge-worker"
)

dockerfile_for_app() {
  case "$1" in
    light-deployer)
      printf 'apps/light-deployer/Dockerfile\n'
      ;;
    *)
      printf 'apps/%s/docker/Dockerfile\n' "$1"
      ;;
  esac
}

assert_line() {
  local expected="$1"
  grep -Fxq -- "$expected" "$DOCKER_LOG" || {
    echo "FAIL: missing Docker invocation: ${expected}" >&2
    exit 1
  }
}

: > "$DOCKER_LOG"
(
  cd "$TEST_ROOT"
  "${REPO_ROOT}/build.sh" 9.8.7 --local --no-cache
)

[[ "$(grep -c '^build ' "$DOCKER_LOG")" -eq "${#APPS[@]}" ]]
if grep -q '^push ' "$DOCKER_LOG"; then
  echo "FAIL: --local attempted to push an image" >&2
  exit 1
fi
for app in "${APPS[@]}"; do
  dockerfile="$(dockerfile_for_app "$app")"
  assert_line "build --no-cache --tag networknt/${app}:9.8.7 --tag networknt/${app}:latest --file ${dockerfile} ."
done

: > "$DOCKER_LOG"
"${REPO_ROOT}/apps/light-gateway/build.sh" 9.8.8 --local --skip-latest
assert_line "build --tag networknt/light-gateway:9.8.8 --file apps/light-gateway/docker/Dockerfile ."
[[ "$(wc -l < "$DOCKER_LOG")" -eq 1 ]]

: > "$DOCKER_LOG"
"${REPO_ROOT}/build.sh" 9.8.9
[[ "$(grep -c '^build ' "$DOCKER_LOG")" -eq "${#APPS[@]}" ]]
[[ "$(grep -c '^push networknt/.*:9\.8\.9$' "$DOCKER_LOG")" -eq "${#APPS[@]}" ]]
[[ "$(grep -c '^push networknt/.*:latest$' "$DOCKER_LOG")" -eq "${#APPS[@]}" ]]
first_push_line="$(grep -n -m1 '^push ' "$DOCKER_LOG" | cut -d: -f1)"
[[ "$first_push_line" -eq $((${#APPS[@]} + 1)) ]]

: > "$DOCKER_LOG"
export DOCKER_FAIL_APP="light-knowledge"
if "${REPO_ROOT}/build.sh" 9.9.0 >/dev/null 2>&1; then
  echo "FAIL: build succeeded after the Docker stub rejected light-knowledge" >&2
  exit 1
fi
unset DOCKER_FAIL_APP
if grep -q '^push ' "$DOCKER_LOG"; then
  echo "FAIL: a failed build published an image" >&2
  exit 1
fi

if "${REPO_ROOT}/build.sh" 9.9.1 --app not-a-release-app >/dev/null 2>&1; then
  echo "FAIL: unknown release app was accepted" >&2
  exit 1
fi

if "${REPO_ROOT}/apps/light-gateway/build.sh" 9.9.2 --local --app light-agent >/dev/null 2>&1; then
  echo "FAIL: app wrapper silently overrode a caller-supplied --app" >&2
  exit 1
fi

echo "PASS: root Docker build orchestration"
