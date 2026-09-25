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

# Images compile inside their builder stage, so host Cargo output must never
# enter the Docker context or invalidate `COPY . .` layers.
if grep -Eq '^!.*target' "${REPO_ROOT}/.dockerignore"; then
  echo "FAIL: .dockerignore re-includes host Cargo output in the Docker build context" >&2
  exit 1
fi

KNOWLEDGE_ADMIN_DOCKERFILE="${REPO_ROOT}/apps/light-knowledge-admin/docker/Dockerfile"
grep -Eq '^FROM rust:[^ ]+-bookworm AS builder$' "$KNOWLEDGE_ADMIN_DOCKERFILE" || {
  echo "FAIL: light-knowledge-admin must compile in a pinned Bookworm builder" >&2
  exit 1
}
grep -Fq 'COPY --from=builder /out/light-knowledge-admin /usr/local/bin/light-knowledge-admin' "$KNOWLEDGE_ADMIN_DOCKERFILE" || {
  echo "FAIL: light-knowledge-admin runtime image does not copy the builder output" >&2
  exit 1
}

APPS=(
  "light-a2a"
  "light-agent"
  "light-deployer"
  "light-gateway"
  "light-identity-issuer"
  "light-workflow"
  "light-workflow-runner"
  "light-knowledge"
  "light-knowledge-admin"
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

assert_build_line() {
  local app="$1"
  local version="$2"
  local dockerfile="$3"
  local expected_regex="$4"
  grep -Eq -- "$expected_regex" "$DOCKER_LOG" || {
    echo "FAIL: missing Docker invocation for ${app}:${version} (${dockerfile})" >&2
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
grep -Eq '^builder prune --force --filter description~=cold-[0-9]+-[0-9]+-$' "$DOCKER_LOG" || {
  echo "FAIL: --no-cache did not prune its cold Cargo cache mounts" >&2
  exit 1
}
for app in "${APPS[@]}"; do
  dockerfile="$(dockerfile_for_app "$app")"
  assert_build_line "$app" "9.8.7" "$dockerfile" \
    "^build --no-cache --tag networknt/${app}:9\\.8\\.7 --build-arg CARGO_CACHE_ID=cold-[0-9]+-[0-9]+-${app} --tag networknt/${app}:latest --file ${dockerfile} \\.$"
done

: > "$DOCKER_LOG"
"${REPO_ROOT}/apps/light-a2a/build.sh" 9.8.8 --local --skip-latest
assert_build_line "light-a2a" "9.8.8" "apps/light-a2a/docker/Dockerfile" \
  '^build --tag networknt/light-a2a:9\.8\.8 --build-arg CARGO_CACHE_ID=warm --file apps/light-a2a/docker/Dockerfile \.$'
[[ "$(wc -l < "$DOCKER_LOG")" -eq 1 ]]

: > "$DOCKER_LOG"
"${REPO_ROOT}/apps/light-gateway/build.sh" 9.8.8 --local --skip-latest
assert_build_line "light-gateway" "9.8.8" "apps/light-gateway/docker/Dockerfile" \
  '^build --tag networknt/light-gateway:9\.8\.8 --build-arg CARGO_CACHE_ID=warm --file apps/light-gateway/docker/Dockerfile \.$'
[[ "$(wc -l < "$DOCKER_LOG")" -eq 1 ]]

: > "$DOCKER_LOG"
"${REPO_ROOT}/apps/light-knowledge-admin/build.sh" 9.8.8 --local --skip-latest
assert_build_line "light-knowledge-admin" "9.8.8" "apps/light-knowledge-admin/docker/Dockerfile" \
  '^build --tag networknt/light-knowledge-admin:9\.8\.8 --build-arg CARGO_CACHE_ID=warm --file apps/light-knowledge-admin/docker/Dockerfile \.$'
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

# --changed orchestration, with the selector replaced by a stub. Selection
# itself is covered by scripts/test_select_changed_apps.py.
mkdir -p "${TEST_ROOT}/changed-bin"
cat > "${TEST_ROOT}/changed-bin/python3" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${SELECTOR_LOG:?}"
printf '%s' "${SELECTOR_OUTPUT:-}"
STUB
cat > "${TEST_ROOT}/changed-bin/cargo" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
chmod +x "${TEST_ROOT}/changed-bin/python3" "${TEST_ROOT}/changed-bin/cargo"
export SELECTOR_LOG="${TEST_ROOT}/selector.log"

: > "$DOCKER_LOG"
: > "$SELECTOR_LOG"
SELECTOR_OUTPUT="" PATH="${TEST_ROOT}/changed-bin:${PATH}" \
  "${REPO_ROOT}/build.sh" 9.9.3 --changed >/dev/null
if [[ -s "$DOCKER_LOG" ]]; then
  echo "FAIL: --changed with no selected images invoked Docker" >&2
  exit 1
fi
for app in "${APPS[@]}"; do
  grep -Fq -- "--only ${app}" "$SELECTOR_LOG" || {
    echo "FAIL: --changed did not offer release app ${app} to the selector" >&2
    exit 1
  }
done
if grep -Fq -- "--only light-knowledge-worker" "$SELECTOR_LOG"; then
  echo "FAIL: --changed offered the optional light-knowledge-worker image" >&2
  exit 1
fi

: > "$DOCKER_LOG"
SELECTOR_OUTPUT=$'light-gateway\nlight-agent' PATH="${TEST_ROOT}/changed-bin:${PATH}" \
  "${REPO_ROOT}/build.sh" 9.9.4 --changed >/dev/null
[[ "$(grep -c '^build ' "$DOCKER_LOG")" -eq 2 ]]
grep -q '^build .*networknt/light-gateway:9\.9\.4' "$DOCKER_LOG"
grep -q '^build .*networknt/light-agent:9\.9\.4' "$DOCKER_LOG"
[[ "$(grep -c '^push ' "$DOCKER_LOG")" -eq 4 ]]

: > "$SELECTOR_LOG"
SELECTOR_OUTPUT="" PATH="${TEST_ROOT}/changed-bin:${PATH}" \
  "${REPO_ROOT}/build.sh" 9.9.5 --changed --app light-knowledge-worker --local >/dev/null
[[ "$(cat "$SELECTOR_LOG")" == "${REPO_ROOT}/scripts/select-changed-apps.py --root ${REPO_ROOT} --only light-knowledge-worker" ]]

echo "PASS: root Docker build orchestration"
