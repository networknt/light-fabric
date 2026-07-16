#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
evidence_dir="$repo_root/target/controller-wire-feature-evidence"
mkdir -p "$evidence_dir"
source "$repo_root/crates/controller-wire/rkyv-v1.profile"

shipping_roots=(light-agent light-deployer light-gateway light-workflow light-workflow-runner)

check_tree() {
  local tree="$1"
  local label="$2"
  grep -Fq 'controller-wire' <<<"$tree" || {
    echo "$label does not contain controller-wire" >&2
    return 1
  }
  grep -Fq "rkyv v$RKYV_VERSION" <<<"$tree" || {
    echo "$label does not resolve pinned rkyv v$RKYV_VERSION" >&2
    return 1
  }
  local feature
  for feature in "${REQUIRED_RKYV_FEATURES[@]}"; do
    grep -Fq "rkyv feature \"$feature\"" <<<"$tree" || {
      echo "$label omits required rkyv feature: $feature" >&2
      return 1
    }
  done
  for feature in "${CONFLICTING_RKYV_FEATURES[@]}"; do
    if grep -Fq "rkyv feature \"$feature\"" <<<"$tree"; then
      echo "$label enables conflicting rkyv feature: $feature" >&2
      return 1
    fi
  done
}

for root in "${shipping_roots[@]}"; do
  tree="$(cargo tree --offline --manifest-path "$repo_root/Cargo.toml" -p "$root" --edges normal,build,features -i rkyv)"
  printf '%s\n' "$tree" >"$evidence_dir/$root.txt"
  check_tree "$tree" "$root release graph"
done

for fixture in big-endian unaligned pointer-width-16 pointer-width-64; do
  manifest="$repo_root/scripts/fixtures/controller-wire-features/$fixture/Cargo.toml"
  tree="$(cargo tree --offline --manifest-path "$manifest" -p "controller-wire-$fixture-fixture" --edges normal,build,features -i rkyv)"
  if fixture_output="$(check_tree "$tree" "$fixture negative fixture" 2>&1)"; then
    echo "$fixture feature-profile negative fixture unexpectedly passed" >&2
    exit 1
  fi
  grep -Fq 'enables conflicting rkyv feature' <<<"$fixture_output" || {
    echo "$fixture negative fixture failed for an unexpected reason" >&2
    printf '%s\n' "$fixture_output" >&2
    exit 1
  }
done

echo "controller-wire rkyv feature profile passed for all shipping runtime graphs"
echo "feature evidence: $evidence_dir"
