#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_NOTES_SCRIPT="${SCRIPT_DIR}/release-notes.sh"
TEST_ROOT="$(mktemp -d)"

cleanup() {
  rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

REMOTE_REPO="${TEST_ROOT}/remote.git"
SEED_REPO="${TEST_ROOT}/seed"
RELEASE_REPO="${TEST_ROOT}/release"

git init --quiet --bare "$REMOTE_REPO"
git init --quiet "$SEED_REPO"
git -C "$SEED_REPO" config user.email release-test@example.com
git -C "$SEED_REPO" config user.name "Release Test"

printf '%s\n' baseline > "${SEED_REPO}/history.txt"
git -C "$SEED_REPO" add history.txt
git -C "$SEED_REPO" commit --quiet -m "fixes #100 baseline"
git -C "$SEED_REPO" tag 0.2.30

printf '%s\n' previous >> "${SEED_REPO}/history.txt"
git -C "$SEED_REPO" commit --quiet -am "fixes #287 previous release"
git -C "$SEED_REPO" tag 0.2.31
git -C "$SEED_REPO" remote add origin "$REMOTE_REPO"
git -C "$SEED_REPO" push --quiet origin HEAD:master --tags

git clone --quiet "$REMOTE_REPO" "$RELEASE_REPO"
git -C "$RELEASE_REPO" config user.email release-test@example.com
git -C "$RELEASE_REPO" config user.name "Release Test"
git -C "$RELEASE_REPO" tag --delete 0.2.31 >/dev/null

printf '%s\n' current >> "${RELEASE_REPO}/history.txt"
git -C "$RELEASE_REPO" commit --quiet -am "fixes #288 current release"

OUTPUT_FILE="${TEST_ROOT}/release-notes.md"
GENERATOR_OUTPUT="$(
  cd "$RELEASE_REPO"
  "$RELEASE_NOTES_SCRIPT" 0.2.32 --output "$OUTPUT_FILE"
)"

grep -Fq 'Commit range: 0.2.31..HEAD' <<<"$GENERATOR_OUTPUT"
grep -Fq 'fixes #288 current release' "$OUTPUT_FILE"
grep -Fq '[#288]' "$OUTPUT_FILE"
if grep -Fq 'fixes #287 previous release' "$OUTPUT_FILE"; then
  echo "FAIL: release notes included commits from the previous release" >&2
  exit 1
fi
if grep -Fq '[#287]' "$OUTPUT_FILE"; then
  echo "FAIL: release notes included issues from the previous release" >&2
  exit 1
fi

echo "PASS: release notes synchronize missing remote tags"
