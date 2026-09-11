#!/usr/bin/env bash
set -euo pipefail
workspace_repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$workspace_repo_root"
case "${1:-}" in
  --host-isolation)
    cargo test -p task-workspace -p light-workspace -p workspace-execution-protocol -- --include-ignored
    ;;
  '')
    cargo test -p task-workspace -p light-workspace -p workspace-execution-protocol
    ;;
  *)
    echo 'usage: run-shared-workspace-gates.sh [--host-isolation]' >&2
    exit 2
    ;;
esac
cargo clippy -p task-workspace -p light-workspace -p workspace-execution-protocol --all-targets -- -D warnings
cargo fmt -p task-workspace -p light-workspace -p workspace-execution-protocol --check
