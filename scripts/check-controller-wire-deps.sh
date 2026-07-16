#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_triple="${CONTROLLER_WIRE_METADATA_TARGET:-x86_64-unknown-linux-gnu}"

forbidden=(
  tokio axum axum-core pingora pingora-core pingora-proxy hyper hyper-util
  sqlx sqlx-core reqwest quinn wtransport web-transport web-transport-quinn
  light-runtime portal-registry light-pingora light-gateway light-agent
  light-deployer light-workflow light-workflow-runner controller-rs
)

dependency_closure() {
  local manifest_path="$1"
  local package="$2"
  cargo metadata --manifest-path "$manifest_path" --format-version 1 \
    --filter-platform "$target_triple" --offline |
    jq -r --arg package "$package" '
      . as $m
      | ($m.packages | map(select(.name == $package)) | first | .id) as $root
      | if $root == null then error("package not found: " + $package) else . end
      | def normal_deps($id):
          $m.resolve.nodes[]
          | select(.id == $id)
          | .deps[]
          | select(any(.dep_kinds[]; .kind == null))
          | .pkg;
        def closure($ids):
          ($ids | unique) as $seen
          | ([$seen[] | normal_deps(.)] | unique
             | map(select(. as $id | $seen | index($id) | not))) as $next
          | if ($next | length) == 0 then $seen else closure($seen + $next) end;
        (closure([$root])[]) as $id
        | $m.packages[]
        | select(.id == $id)
        | .name
    ' | sort -u
}

check_package() {
  local manifest_path="$1"
  local package="$2"
  local closure
  closure="$(dependency_closure "$manifest_path" "$package")"
  printf '%s\n' "$closure"
  local dependency
  for dependency in "${forbidden[@]}"; do
    if grep -Fxq "$dependency" <<<"$closure"; then
      echo "forbidden normal dependency in $package closure: $dependency" >&2
      return 1
    fi
  done
}

check_package "$repo_root/Cargo.toml" controller-wire

fixture="$repo_root/scripts/fixtures/controller-wire-deps/forbidden/Cargo.toml"
if fixture_output="$(check_package "$fixture" controller-wire-forbidden-fixture 2>&1)"; then
  echo "dependency-purity negative fixture unexpectedly passed" >&2
  exit 1
fi
grep -Fq 'forbidden normal dependency in controller-wire-forbidden-fixture closure: tokio' \
  <<<"$fixture_output" || {
  echo "dependency-purity negative fixture failed for an unexpected reason" >&2
  printf '%s\n' "$fixture_output" >&2
  exit 1
}

echo "controller-wire dependency purity gate passed (negative fixture rejected)"
