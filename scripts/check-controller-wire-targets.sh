#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
targets=(x86_64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-apple-darwin)
installed="$(rustup target list --installed)"

for target in "${targets[@]}"; do
  grep -Fxq "$target" <<<"$installed" || {
    echo "required Phase 2 conformance target is not installed: $target" >&2
    exit 1
  }
  cargo check --offline --manifest-path "$repo_root/Cargo.toml" \
    -p controller-wire --tests --target "$target"
done

echo "controller-wire fixtures and validated parsers compile for x86_64 and aarch64 targets"
