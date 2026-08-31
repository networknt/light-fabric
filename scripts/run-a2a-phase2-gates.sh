#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 postgresql://.../DISPOSABLE_DATABASE" >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

for repo in portal-db light-portal genai-command genai-query portal-view light-fabric portal-service light-portal-event; do
  git -C "$workspace_root/$repo" diff --check
done

"$workspace_root/portal-db/postgres/tests/run-a2a-phase2-authoring-schema-gate.sh" "$1"

(
  cd "$workspace_root/light-portal"
  mvn -pl common-util -Dtest=EventTypeUtilTest,EntityCreationPhase0ContractTest test
  mvn -pl db-provider -am -Dtest=A2aPublicationSupportTest,ReplayPolicyRegistryTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
  mvn -pl common-util,db-provider -am -DskipTests install
)
(
  cd "$workspace_root/genai-command"
  mvn -Dtest=A2aAuthoringCommandTest test
  mvn -DskipTests -Dmaven.javadoc.skip=true package
)
(
  cd "$workspace_root/genai-query"
  mvn -DskipTests -Dmaven.javadoc.skip=true package
)
(
  cd "$workspace_root/portal-view"
  npm run build
)
(
  cd "$workspace_root/portal-service"
  cargo fmt --all -- --check
  cargo test -p light-oauth a2a_
  cargo check -p light-oauth
)
(
  cd "$fabric_root"
  cargo fmt --all -- --check
  cargo test -p a2a-protocol -p light-a2a --lib
  cargo check -p light-agent -p light-gateway
)
(
  cd "$workspace_root/light-portal-event"
  files=(
    genai/20260830-a2a-router-config.json
    genai/20260830-a2a-phase2-config-properties.json
    genai/20260830-a2a-phase2-product-mappings.json
    genai/20260830-a2a-runtime-product-mappings.json
    genai/20260830-agent-runtime-identity-cutover.json
    genai/20260830-agent-runtime-identity-mappings.json
  )
  jq -e -s '
    add
    | length == 90
      and (all(.[]; .nonce == "0"))
      and (all(.[]; .aggregateversion == .data.newAggregateVersion))
      and ((group_by(.id) | all(.[]; length == 1)))
  ' "${files[@]}" >/dev/null
)

echo "A2A Phase 2 gates PASS"
