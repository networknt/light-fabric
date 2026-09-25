#!/usr/bin/env bash
set -euo pipefail

if [[ "${DEBUG:-false}" == "true" ]]; then
  set -x
fi

readonly IMAGE_NAMESPACE="networknt"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="${SCRIPT_DIR}"

VERSION=""
APP="all"
APP_SELECTED=false
LOCAL_BUILD=false
NO_CACHE=false
SKIP_LATEST=false
CHANGED_ONLY=false

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

# Compatibility/CLI image only. It is never part of the default release set.
OPTIONAL_APPS=("light-knowledge-worker")

show_help() {
  local error="${1:-}"

  echo " "
  if [[ -n "$error" ]]; then
    echo "Error: ${error}"
    echo " "
  fi
  echo "    build.sh [VERSION] [-a|--app APP] [-l|--local] [--changed] [--no-cache] [--skip-latest]"
  echo " "
  echo "    where [VERSION] is the Docker image version to build and publish"
  echo "          [-a|--app APP] optionally builds one release app instead of all apps"
  echo "          [-l|--local] builds images locally without pushing"
  echo "          [--changed] selects images affected by staged, unstaged, and untracked files"
  echo "          [--no-cache] builds images without using the Docker build cache"
  echo "          [--skip-latest] does not create or push latest tags"
  echo " "
  echo "    examples:"
  echo "          ./build.sh 0.3.0"
  echo "          ./build.sh 0.3.0 --local"
  echo "          ./build.sh 0.3.0 --changed --local"
  echo "          ./build.sh 0.3.0 --app light-gateway --no-cache"
  echo " "
  echo "    release apps: ${APPS[*]}"
  echo "    optional compatibility apps: ${OPTIONAL_APPS[*]}"
  echo " "
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

contains_app() {
  local candidate="$1"
  local release_app

  for release_app in "${APPS[@]}"; do
    if [[ "$release_app" == "$candidate" ]]; then
      return 0
    fi
  done
  for release_app in "${OPTIONAL_APPS[@]}"; do
    if [[ "$release_app" == "$candidate" ]]; then
      return 0
    fi
  done
  return 1
}

dockerfile_for_app() {
  case "$1" in
    light-a2a|light-agent|light-gateway|light-identity-issuer|light-workflow|light-knowledge-admin)
      printf 'apps/%s/docker/Dockerfile\n' "$1"
      ;;
    light-deployer)
      printf 'apps/light-deployer/Dockerfile\n'
      ;;
    light-workflow-runner|light-knowledge|light-knowledge-worker)
      printf 'apps/%s/docker/Dockerfile\n' "$1"
      ;;
    *)
      fail "No Dockerfile configured for app: $1"
      ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      show_help
      exit 0
      ;;
    -a|--app|-s|--service)
      [[ $# -ge 2 ]] || fail "$1 requires an app name"
      if $APP_SELECTED; then
        fail "Only one --app/--service selection may be supplied"
      fi
      APP="$2"
      APP_SELECTED=true
      shift 2
      ;;
    -l|--local)
      LOCAL_BUILD=true
      shift
      ;;
    --changed)
      CHANGED_ONLY=true
      shift
      ;;
    --no-cache)
      NO_CACHE=true
      shift
      ;;
    --skip-latest)
      SKIP_LATEST=true
      shift
      ;;
    -*)
      show_help "Invalid option: $1"
      exit 1
      ;;
    *)
      if [[ -z "$VERSION" ]]; then
        VERSION="$1"
      else
        show_help "Invalid option: $1"
        exit 1
      fi
      shift
      ;;
  esac
done

[[ -n "$VERSION" ]] || fail "[VERSION] parameter is missing"
command -v docker >/dev/null 2>&1 || fail "Missing required command: docker"
if $CHANGED_ONLY; then
  command -v cargo >/dev/null 2>&1 || fail "Missing required command: cargo"
  command -v python3 >/dev/null 2>&1 || fail "Missing required command: python3"
fi

if [[ "$APP" == "all" ]]; then
  BUILD_APPS=("${APPS[@]}")
else
  contains_app "$APP" || fail "Unknown release app: $APP"
  BUILD_APPS=("$APP")
fi

declare -a BUILD_ARGS=()
if $NO_CACHE; then
  BUILD_ARGS+=(--no-cache)
fi

cd "$REPO_ROOT"

if $CHANGED_ONLY; then
  selector_args=(--root "$REPO_ROOT")
  if $APP_SELECTED; then
    selector_args+=(--only "$APP")
  fi
  changed_output="$(python3 "$SCRIPT_DIR/scripts/select-changed-apps.py" "${selector_args[@]}")" \
    || fail "Unable to select changed apps"
  if [[ -z "$changed_output" ]]; then
    echo "No selected images are affected by uncommitted files; nothing to build or publish"
    exit 0
  fi
  mapfile -t CHANGED_APPS <<< "$changed_output"
  BUILD_APPS=("${CHANGED_APPS[@]}")
  echo "Changed image selection: ${BUILD_APPS[*]}"
fi

# Finish every local build before publishing any tag. This prevents a compile
# failure in a later app from publishing an incomplete release unnecessarily.
for release_app in "${BUILD_APPS[@]}"; do
  dockerfile="$(dockerfile_for_app "$release_app")"
  [[ -f "$dockerfile" ]] || fail "Missing Dockerfile: $dockerfile"

  version_image="${IMAGE_NAMESPACE}/${release_app}:${VERSION}"
  docker_args=(build "${BUILD_ARGS[@]}" --tag "$version_image")
  if $NO_CACHE; then
    cache_id="cold-${BASHPID}-${RANDOM}-${release_app}"
  else
    cache_id="warm"
  fi
  docker_args+=(--build-arg "CARGO_CACHE_ID=${cache_id}")
  if ! $SKIP_LATEST; then
    docker_args+=(--tag "${IMAGE_NAMESPACE}/${release_app}:latest")
  fi
  docker_args+=(--file "$dockerfile" .)

  echo "Building ${version_image}"
  DOCKER_BUILDKIT=1 docker "${docker_args[@]}"
done

if $LOCAL_BUILD; then
  echo "Built all selected images locally; skipping Docker Hub publish"
  exit 0
fi

for release_app in "${BUILD_APPS[@]}"; do
  version_image="${IMAGE_NAMESPACE}/${release_app}:${VERSION}"
  echo "Pushing ${version_image}"
  docker push "$version_image"
done

if ! $SKIP_LATEST; then
  for release_app in "${BUILD_APPS[@]}"; do
    latest_image="${IMAGE_NAMESPACE}/${release_app}:latest"
    echo "Pushing ${latest_image}"
    docker push "$latest_image"
  done
fi

echo "Published all selected Docker images with version ${VERSION}"
