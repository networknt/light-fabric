#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)
event_file="$workspace_root/light-portal-event/workflow/20260824-light-workflow-phase1b-typed-config.json"

cd "$repo_root"
./scripts/run-light-workflow-config-controller-phase0-gate.sh
cargo test -p light-workflow configuration::tests
cargo test -p light-workflow managed_agent_credentials_reject_literal_references
cargo check -p light-workflow --all-targets

if rg -n 'std::env::var|env::var' \
  apps/light-workflow/src/main.rs \
  apps/light-workflow/src/rule_api.rs \
  apps/light-workflow/src/consumer.rs \
  apps/light-workflow/src/artifact_store.rs; then
  echo "Phase 1b forbids direct environment reads outside the typed candidate and documented secret resolvers." >&2
  exit 1
fi

if rg -n 'env::var\(key\)|env::var\("LIGHT_WORKFLOW_AGENT_.*BASE_URL|env::var\("COMPATIBLE_BASE_URL' \
  apps/light-workflow/src/executor.rs; then
  echo "Agent provider base URLs must come from the typed Config Server candidate." >&2
  exit 1
fi

jq -e '
  type == "array" and length == 50 and
  ([.[] | select(.type == "ConfigCreatedEvent")] | length == 1) and
  ([.[] | select(.type == "ConfigPropertyCreatedEvent")] | length == 21) and
  ([.[] | select(.type == "ProductVersionConfigCreatedEvent")] | length == 1) and
  ([.[] | select(.type == "ProductVersionConfigPropertyCreatedEvent")] | length == 21) and
  ([.[] | select(.type == "ConfigInstanceCreatedEvent")] | length == 6) and
  all(.[]; .nonce == "0" and .aggregateversion == 1)
' "$event_file" >/dev/null

if jq -r '.. | strings' "$event_file" | rg -i \
  'postgres(ql)?://|bearer[[:space:]]+[A-Za-z0-9._-]+|api[_-]?key[=:][^ ]|literal:'; then
  echo "Phase 1b event publication contains a credential-shaped value." >&2
  exit 1
fi

if rg -n -i \
  'postgres(ql)?://|bearer[[:space:]]+[A-Za-z0-9._-]+|api[_-]?key[=:][^ ]|literal:' \
  apps/light-workflow/config/workflow.yml \
  apps/light-workflow/config-contract/fixtures; then
  echo "Phase 1b configuration/cache fixtures contain a credential-shaped value." >&2
  exit 1
fi

python3 - "$event_file" <<'PY'
import json, sys, uuid
events = json.load(open(sys.argv[1], encoding="utf-8"))
ids = []
for event in events:
    ids.append(event["id"])
    if event["type"] == "ConfigCreatedEvent":
        ids.append(event["data"]["configId"])
    if event["type"] == "ConfigPropertyCreatedEvent":
        ids.append(event["data"]["propertyId"])
assert len(ids) == len(set(ids)), "generated UUIDs are not unique"
for value in ids:
    parsed = uuid.UUID(value)
    assert parsed.version == 7 and parsed.variant == uuid.RFC_4122, value
subjects = [(event["subject"], event["aggregateversion"]) for event in events]
assert len(subjects) == len(set(subjects)), "duplicate aggregate version"
PY

docker compose -f "$workspace_root/portal-config-dev/docker-compose.yml" config --quiet
docker compose \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose-rust.yml" \
  config --quiet
docker compose -f "$workspace_root/light-portal-install/docker-compose.yml" config --quiet

echo "Light Workflow Config Server Phase 1b gate passed."
