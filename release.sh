#!/usr/bin/env bash
set -euo pipefail

if [ "${DEBUG:-false}" = "true" ]; then
  set -x
fi

VERSION=""
LOCAL_BUILD=false
SKIP_BUILD=false
INSTALL_TARGETS=true
DIST_DIR="${DIST_DIR:-dist}"

TARGETS=(
  "x86_64-unknown-linux-gnu"
  "x86_64-unknown-linux-musl"
)

APPS=(
  "light-agent"
  "light-deployer"
  "light-gateway"
  "light-workflow"
)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${SCRIPT_DIR}"

show_help() {
  local error="${1:-}"

  echo " "
  if [[ -n "$error" ]]; then
    echo "Error: ${error}"
    echo " "
  fi
  echo "    release.sh [VERSION] [-l|--local] [--skip-build] [--no-target-add] [--dist DIR]"
  echo " "
  echo "    where [VERSION] is the GitHub release tag to build and publish"
  echo "          [-l|--local] builds and packages locally without uploading to GitHub"
  echo "          [--skip-build] packages existing target binaries without rebuilding"
  echo "          [--no-target-add] skips rustup target installation"
  echo "          [--dist DIR] writes release archives to DIR instead of dist"
  echo " "
  echo "    targets:"
  echo "          x86_64-unknown-linux-gnu"
  echo "          x86_64-unknown-linux-musl"
  echo " "
  echo "    examples:"
  echo "          ./release.sh v0.1.0"
  echo "          ./release.sh v0.1.0 --local"
  echo "          ./release.sh v0.1.0 --skip-build"
  echo " "
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

require_command() {
  local command_name="$1"
  command -v "$command_name" >/dev/null 2>&1 || fail "Missing required command: ${command_name}"
}

require_musl_toolchain() {
  command -v x86_64-linux-musl-gcc >/dev/null 2>&1 && return 0

  fail "Missing x86_64-linux-musl-gcc. Install the musl toolchain first, for example: sudo apt-get install -y musl-tools pkg-config"
}

setup_build_env() {
  if command -v ccache >/dev/null 2>&1; then
    export CCACHE_DIR="${CCACHE_DIR:-${REPO_ROOT}/target/ccache}"
    mkdir -p "$CCACHE_DIR"
  fi
}

build_target() {
  local target="$1"
  local cargo_args=()

  for app in "${APPS[@]}"; do
    cargo_args+=(--bin "$app")
  done

  if $INSTALL_TARGETS; then
    rustup target add "$target"
  fi

  if [[ "$target" == "x86_64-unknown-linux-musl" ]]; then
    require_musl_toolchain
  fi

  echo "Building Light-Fabric apps for ${target}"
  cargo build --locked --release --target "$target" "${cargo_args[@]}"
}

package_target() {
  local target="$1"
  local artifact="light-fabric-${VERSION}-${target}"
  local staging_dir="${DIST_DIR}/${artifact}"
  local archive="${DIST_DIR}/${artifact}.tar.gz"

  echo "Packaging ${archive}"
  rm -rf -- "$staging_dir" "$archive"
  mkdir -p "$staging_dir"

  for app in "${APPS[@]}"; do
    local binary="target/${target}/release/${app}"
    [[ -x "$binary" ]] || fail "Missing release binary: ${binary}"
    cp "$binary" "$staging_dir/"
  done

  tar -C "$DIST_DIR" -czf "$archive" "$artifact"
  ARCHIVES+=("$archive")
}

publish_release() {
  require_command gh

  if gh release view "$VERSION" >/dev/null 2>&1; then
    echo "Uploading artifacts to existing GitHub release ${VERSION}"
    gh release upload "$VERSION" "${ARCHIVES[@]}" --clobber
  else
    echo "Creating GitHub release ${VERSION}"
    gh release create "$VERSION" "${ARCHIVES[@]}" \
      --title "$VERSION" \
      --notes "Light-Fabric Linux release binaries"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      show_help
      exit 0
      ;;
    -l|--local)
      LOCAL_BUILD=true
      shift
      ;;
    --skip-build)
      SKIP_BUILD=true
      shift
      ;;
    --no-target-add)
      INSTALL_TARGETS=false
      shift
      ;;
    --dist)
      [[ $# -ge 2 ]] || fail "--dist requires a directory"
      DIST_DIR="$2"
      shift 2
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

if [[ -z "$VERSION" ]]; then
  show_help "[VERSION] parameter is missing"
  exit 1
fi

require_command cargo
require_command cp
require_command mkdir
require_command rm
require_command tar

if ! $LOCAL_BUILD; then
  require_command gh
fi

if $INSTALL_TARGETS && ! $SKIP_BUILD; then
  require_command rustup
fi

if ! $SKIP_BUILD; then
  for target in "${TARGETS[@]}"; do
    if [[ "$target" == "x86_64-unknown-linux-musl" ]]; then
      require_musl_toolchain
    fi
  done
fi

cd "$REPO_ROOT"
mkdir -p "$DIST_DIR"
setup_build_env

declare -a ARCHIVES=()

for target in "${TARGETS[@]}"; do
  if ! $SKIP_BUILD; then
    build_target "$target"
  fi
  package_target "$target"
done

echo "Release archives:"
for archive in "${ARCHIVES[@]}"; do
  echo "  ${archive}"
done

if $LOCAL_BUILD; then
  echo "Skipping GitHub release upload due to local build flag (-l or --local)"
else
  publish_release
fi
