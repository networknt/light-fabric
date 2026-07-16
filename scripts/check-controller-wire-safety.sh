#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="$repo_root/crates/controller-wire/src"

if rg -n 'access_unchecked|from_bytes_unchecked|deserialize_unchecked' "$source_root"; then
  echo "unchecked archived access is forbidden in controller-wire production modules" >&2
  exit 1
fi

if rg -n '\bunsafe\b' "$source_root" | rg -v '#!\[forbid\(unsafe_code\)\]|never performs unchecked'; then
  echo "unsafe code is forbidden in controller-wire production modules" >&2
  exit 1
fi

echo "controller-wire production archive safety gate passed"
