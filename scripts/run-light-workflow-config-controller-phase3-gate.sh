#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)

cd "$repo_root"
./scripts/run-light-workflow-config-controller-phase2-gate.sh
cargo test --locked -p light-runtime registry_failure_is_fail_closed_when_tolerant_start_is_disabled
cargo test --locked -p portal-registry metadata_update_refreshes_reconnect_registration_while_disconnected
cargo test --locked -p light-workflow service_runtime::tests

for marker in \
  'WorkflowOperationalMetadata::new' \
  'registration_tags(&self)' \
  'light-workflow-controller-observer' \
  '.register(Arc::new(self.operational_metadata.clone()))'; do
  if ! rg -Fq "$marker" apps/light-workflow/src/main.rs; then
    echo "Phase 3 workflow registration marker is missing: $marker" >&2
    exit 1
  fi
done

for compose_file in \
  "$workspace_root/portal-config-dev/docker-compose.yml" \
  "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" \
  "$workspace_root/light-portal-install/docker-compose.yml"; do
  for setting in \
    'server.enableRegistry: "true"' \
    'server.startOnRegistryFailure: "false"' \
    'portalRegistry.portalUrl: "https://controller:8438"' \
    'server.advertisedAddress: "light-workflow"'; do
    if ! rg -Fq "$setting" "$compose_file"; then
      echo "Phase 3 managed registry setting is missing from $compose_file: $setting" >&2
      exit 1
    fi
  done
done

cargo test --locked --manifest-path "$workspace_root/controller-rs/Cargo.toml" \
  --test postgres_runtime_instance
for test_name in \
  microservice_explicit_profile_rejects_expired_and_wrong_audience_tokens \
  explicit_rkyv_reconnect_storm_keeps_one_live_registration_per_generation \
  disconnect_and_reconnect_reuses_runtime_instance_for_same_business_key \
  stale_socket_disconnect_does_not_remove_reused_runtime_instance \
  discovery_protocol_filter_updates_after_metadata_change; do
  cargo test --locked --manifest-path "$workspace_root/controller-rs/Cargo.toml" \
    --test websocket_flows "$test_name"
done

(cd "$workspace_root/light-portal" && mvn -q -pl db-provider -am -DskipTests compile)
(cd "$workspace_root/portal-view" && npm test -- --run src/pages/controller/CtrlPaneDashboard.test.ts)

if [[ -n "${PORTAL_DB_PHASE3_TEST_URL:-}" ]]; then
  "$workspace_root/portal-db/postgres/tests/run-runtime-instance-operational-metadata-schema-gate.sh" \
    "$PORTAL_DB_PHASE3_TEST_URL"
else
  echo "PORTAL_DB_PHASE3_TEST_URL is unset; skipping the disposable-database equivalence gate."
fi

docker compose -f "$workspace_root/portal-config-dev/docker-compose.yml" config --quiet
docker compose -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" config --quiet
docker compose -f "$workspace_root/light-portal-install/docker-compose.yml" config --quiet

echo "Light Workflow Config Server/controller Phase 3 gate passed."
