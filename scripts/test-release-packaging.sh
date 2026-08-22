#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEST_ROOT="$(mktemp -d)"

cleanup() {
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

FIXTURE_REPO="${TEST_ROOT}/light-fabric"
mkdir -p "${FIXTURE_REPO}/scripts"
cp "${REPO_ROOT}/release.sh" "${FIXTURE_REPO}/release.sh"
cp "${REPO_ROOT}/scripts/release-notes.sh" "${FIXTURE_REPO}/scripts/release-notes.sh"
chmod +x "${FIXTURE_REPO}/release.sh" "${FIXTURE_REPO}/scripts/release-notes.sh"

mapfile -t APPS < <(
  sed -n '/^APPS=(/,/^)/p' "${FIXTURE_REPO}/release.sh" |
    sed -n 's/^[[:space:]]*"\([^"]*\)"/\1/p'
)
TARGETS=(
  "x86_64-unknown-linux-gnu"
  "x86_64-unknown-linux-musl"
)

[[ "${#APPS[@]}" -eq 8 ]] || {
  echo "FAIL: expected eight release binaries, found ${#APPS[@]}" >&2
  exit 1
}

git init --quiet "$FIXTURE_REPO"
git -C "$FIXTURE_REPO" config user.email release-package-test@example.com
git -C "$FIXTURE_REPO" config user.name "Release Package Test"
printf '%s\n' baseline > "${FIXTURE_REPO}/history.txt"
git -C "$FIXTURE_REPO" add .
git -C "$FIXTURE_REPO" commit --quiet -m "baseline"
printf '%s\n' release >> "${FIXTURE_REPO}/history.txt"
git -C "$FIXTURE_REPO" commit --quiet -am "release binaries"

for target in "${TARGETS[@]}"; do
  binary_dir="${FIXTURE_REPO}/target/${target}/release"
  mkdir -p "$binary_dir"
  for app in "${APPS[@]}"; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "${binary_dir}/${app}"
    chmod +x "${binary_dir}/${app}"
  done
done

(
  cd "$FIXTURE_REPO"
  ./release.sh 9.8.7 --local --skip-build --no-target-add --from HEAD~1
)

for target in "${TARGETS[@]}"; do
  archive="${FIXTURE_REPO}/dist/light-fabric-9.8.7-${target}.tar.gz"
  [[ -f "$archive" ]] || {
    echo "FAIL: missing archive ${archive}" >&2
    exit 1
  }

  diff -u \
    <(printf '%s\n' "${APPS[@]}" | sort) \
    <(tar -tzf "$archive" | awk -F/ 'NF == 2 && $2 != "" { print $2 }' | sort)
done

echo "PASS: release archives contain every configured binary"
