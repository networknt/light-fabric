#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$repo_root/.." && pwd)"

cd "$repo_root"
cargo test -p light-runtime --lib
cargo test -p light-workflow config_bootstrap --bin light-workflow
cargo check -p light-workflow --all-targets

cd "$workspace_root/portal-service"
cargo test -p portal-core config_values --lib
cargo check -p config-server

events="$workspace_root/light-portal-event/workflow/20260824-light-workflow-phase1a-config.json"
jq -e '
  type == "array" and
  length == 22 and
  ([.[].id] | unique | length) == 22 and
  ([.[].subject] | unique | length) == 22 and
  all(.[]; .nonce == "0" and .aggregateversion == 1) and
  ([.[] | tostring | ascii_downcase |
    test("bearer |password|api[_-]?key|private[_-]?key|database_url")]
    | any | not)
' "$events" >/dev/null

docker compose -f "$workspace_root/portal-config-dev/docker-compose.yml" config --quiet
docker compose \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose-rust.yml" \
  config --quiet
docker compose -f "$workspace_root/light-portal-install/docker-compose.yml" config --quiet

echo "Light Workflow Config Server Phase 1a gate passed."
