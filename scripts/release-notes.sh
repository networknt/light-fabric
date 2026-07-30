#!/usr/bin/env bash
set -euo pipefail

VERSION=""
FROM_REF=""
TARGET_REF=""
OUTPUT_FILE=""
CHANGELOG_FILE="CHANGELOG.md"
UPDATE_CHANGELOG=false
INCLUDE_MERGES=false
declare -a ARTIFACTS=()
declare -a EXPLICIT_ISSUES=()

fail() {
  echo "Error: $*" >&2
  exit 1
}

show_help() {
  cat <<'EOF'
Usage: scripts/release-notes.sh VERSION [options]

Options:
  --from TAG             Start after TAG instead of auto-detecting it
  --target-ref REF       End at REF instead of VERSION (if tagged) or HEAD
  --include-merges       Include merge commits in the change list
  --issue NUMBER         Add an addressed issue; may be repeated
  --artifact NAME        Add an artifact to the GitHub notes; may be repeated
  --output FILE          Write GitHub release notes to FILE
  --update-changelog     Insert or replace VERSION in CHANGELOG.md
  --changelog FILE       Use a different changelog path (primarily for tests)
  -h, --help             Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      show_help
      exit 0
      ;;
    --from)
      [[ $# -ge 2 ]] || fail "--from requires a git ref"
      FROM_REF="$2"
      shift 2
      ;;
    --target|--target-ref)
      [[ $# -ge 2 ]] || fail "--target-ref requires a git ref"
      TARGET_REF="$2"
      shift 2
      ;;
    --include-merges)
      INCLUDE_MERGES=true
      shift
      ;;
    --issue)
      [[ $# -ge 2 ]] || fail "--issue requires an issue number"
      issue_number="${2#\#}"
      [[ "$issue_number" =~ ^[0-9]+$ ]] || fail "--issue requires a numeric issue number"
      EXPLICIT_ISSUES+=("$issue_number")
      shift 2
      ;;
    --artifact)
      [[ $# -ge 2 ]] || fail "--artifact requires a name"
      ARTIFACTS+=("$2")
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || fail "--output requires a file"
      OUTPUT_FILE="$2"
      shift 2
      ;;
    --update-changelog)
      UPDATE_CHANGELOG=true
      shift
      ;;
    --changelog)
      [[ $# -ge 2 ]] || fail "--changelog requires a file"
      CHANGELOG_FILE="$2"
      shift 2
      ;;
    -*)
      fail "Invalid option: $1"
      ;;
    *)
      [[ -z "$VERSION" ]] || fail "Unexpected argument: $1"
      VERSION="$1"
      shift
      ;;
  esac
done

[[ -n "$VERSION" ]] || fail "VERSION is required"
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || fail "Run this script in a git worktree"

if [[ -z "$FROM_REF" ]] && git remote get-url origin >/dev/null 2>&1; then
  echo "Synchronizing release tags from origin"
  git fetch --tags origin || fail "Unable to synchronize release tags from origin; use --from REF to select the range explicitly"
fi

if [[ -z "$TARGET_REF" ]]; then
  if git rev-parse --verify --quiet "refs/tags/${VERSION}^{commit}" >/dev/null; then
    TARGET_REF="$VERSION"
  else
    TARGET_REF="HEAD"
  fi
fi
git rev-parse --verify --quiet "${TARGET_REF}^{commit}" >/dev/null || fail "Unknown target ref: ${TARGET_REF}"

if [[ -z "$FROM_REF" ]]; then
  while IFS= read -r candidate; do
    [[ "$candidate" == "$VERSION" ]] && continue
    if [[ "$candidate" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+([.-].*)?$ ]]; then
      FROM_REF="$candidate"
      break
    fi
  done < <(git tag --merged "$TARGET_REF" --sort=-version:refname)
fi

if [[ -n "$FROM_REF" ]]; then
  git rev-parse --verify --quiet "${FROM_REF}^{commit}" >/dev/null || fail "Unknown starting ref: ${FROM_REF}"
  FROM_COMMIT="$(git rev-parse "${FROM_REF}^{commit}")"
  TARGET_COMMIT="$(git rev-parse "${TARGET_REF}^{commit}")"
  if [[ "$FROM_COMMIT" == "$TARGET_COMMIT" ]]; then
    fail "Starting ref ${FROM_REF} resolves to the same commit as target ${TARGET_REF}; choose an earlier --from tag"
  fi
  COMMIT_RANGE="${FROM_REF}..${TARGET_REF}"
else
  COMMIT_RANGE="$TARGET_REF"
fi

REPO_SLUG="networknt/light-fabric"
if origin_url="$(git config --get remote.origin.url 2>/dev/null)"; then
  case "$origin_url" in
    git@github.com:*) REPO_SLUG="${origin_url#git@github.com:}" ;;
    https://github.com/*) REPO_SLUG="${origin_url#https://github.com/}" ;;
  esac
  REPO_SLUG="${REPO_SLUG%.git}"
fi

if git rev-parse --verify --quiet "refs/tags/${VERSION}^{commit}" >/dev/null; then
  RELEASE_DATE="$(git log -1 --format=%cs "$VERSION")"
else
  RELEASE_DATE="$(date -u +%F)"
fi

OUTPUT_FILE="${OUTPUT_FILE:-dist/release-notes-${VERSION}.md}"
mkdir -p "$(dirname "$OUTPUT_FILE")"

declare -a LOG_ARGS=(--reverse --pretty=format:'- %s (`%h`)')
declare -a ISSUE_LOG_ARGS=(--reverse --pretty=format:'%s%n%b')
if ! $INCLUDE_MERGES; then
  LOG_ARGS+=(--no-merges)
  ISSUE_LOG_ARGS+=(--no-merges)
fi

CHANGES="$(git log "${LOG_ARGS[@]}" "$COMMIT_RANGE")"
ISSUE_TEXT="$(git log "${ISSUE_LOG_ARGS[@]}" "$COMMIT_RANGE")"

declare -a ISSUE_NUMBERS=()
while IFS= read -r issue_number; do
  [[ -n "$issue_number" ]] && ISSUE_NUMBERS+=("$issue_number")
done < <(
  {
    printf '%s\n' "$ISSUE_TEXT" \
      | grep -Eoi '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]]+#[0-9]+' \
      | grep -Eo '#[0-9]+' || true
    printf '%s\n' "$ISSUE_TEXT" \
      | grep -Eo 'https://github\.com/[^/[:space:]]+/[^/[:space:]]+/issues/[0-9]+' \
      | awk -v prefix="https://github.com/${REPO_SLUG}/issues/" 'index($0, prefix) == 1' || true
    printf '%s\n' "${EXPLICIT_ISSUES[@]:-}"
  } | sed -E 's#^.*/issues/##; s/^#//' | grep -E '^[0-9]+$' | awk '!seen[$0]++'
)

SECTION_FILE="$(mktemp)"
CLEANED_CHANGELOG="$(mktemp)"
CHANGELOG_TMP=""
cleanup() {
  rm -f "$SECTION_FILE" "$CLEANED_CHANGELOG"
  [[ -z "$CHANGELOG_TMP" ]] || rm -f "$CHANGELOG_TMP"
}
trap cleanup EXIT

{
  printf '## %s - %s\n\n' "$VERSION" "$RELEASE_DATE"
  printf '### Changes\n\n'
  if [[ -n "$CHANGES" ]]; then
    printf '%s\n' "$CHANGES"
  else
    printf '%s\n' '- No changes recorded for this release range.'
  fi
  printf '\n### Issues Addressed\n\n'
  if [[ ${#ISSUE_NUMBERS[@]} -gt 0 ]]; then
    for issue_number in "${ISSUE_NUMBERS[@]}"; do
      printf -- '- [#%s](https://github.com/%s/issues/%s)\n' "$issue_number" "$REPO_SLUG" "$issue_number"
    done
  else
    printf '%s\n' '- No issue references found in the release commit messages.'
  fi
} > "$SECTION_FILE"

{
  cat "$SECTION_FILE"
  if [[ ${#ARTIFACTS[@]} -gt 0 ]]; then
    printf '\n### Artifacts\n\n'
    for artifact in "${ARTIFACTS[@]}"; do
      printf -- '- `%s`\n' "$artifact"
    done
  fi
} > "$OUTPUT_FILE"

if $UPDATE_CHANGELOG; then
  if [[ -f "$CHANGELOG_FILE" ]]; then
    awk -v prefix="## ${VERSION} -" '
      NR == 1 && $0 == "# Changelog" { next }
      index($0, prefix) == 1 { skipping = 1; next }
      skipping && /^## / { skipping = 0 }
      !skipping { print }
    ' "$CHANGELOG_FILE" > "$CLEANED_CHANGELOG"
  else
    : > "$CLEANED_CHANGELOG"
  fi

  changelog_dir="$(dirname "$CHANGELOG_FILE")"
  mkdir -p "$changelog_dir"
  CHANGELOG_TMP="$(mktemp "${changelog_dir}/.CHANGELOG.XXXXXX")"
  {
    printf '# Changelog\n\n'
    cat "$SECTION_FILE"
    printf '\n'
    sed '/./,$!d' "$CLEANED_CHANGELOG"
  } > "$CHANGELOG_TMP"
  mv "$CHANGELOG_TMP" "$CHANGELOG_FILE"
  CHANGELOG_TMP=""
fi

echo "Generated release notes: ${OUTPUT_FILE}"
echo "Commit range: ${COMMIT_RANGE}"
if $UPDATE_CHANGELOG; then
  echo "Updated changelog: ${CHANGELOG_FILE}"
fi
