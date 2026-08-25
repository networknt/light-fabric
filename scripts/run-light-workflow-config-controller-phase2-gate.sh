#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)

cd "$repo_root"
./scripts/run-light-workflow-config-controller-phase1b-gate.sh
cargo test --locked -p light-runtime lifecycle::tests
cargo test --locked -p light-runtime start_accepts_an_already_validated_configuration_candidate
cargo test --locked -p light-runtime duplicate_module_registration_aborts_every_built_module
cargo test --locked -p light-axum listener_bind_failure_unwinds_participants_started_by_the_app
cargo test --locked -p light-axum admission_rejects_new_requests_and_holds_stream_permit_until_body_finishes
cargo test --locked -p light-workflow service_runtime::tests
cargo test --locked -p light-workflow readiness_reports_background_failure_without_changing_liveness
cargo test --locked -p light-workflow --all-targets

for marker in \
  'AxumTransport::new(app)' \
  '.with_prepared_config(config_activation.runtime_config)' \
  '.run_until_shutdown(watcher)' \
  'light-workflow-event-consumer' \
  'light-workflow-task-executor' \
  'light-workflow-result-reconciler'; do
  if ! rg -Fq "$marker" apps/light-workflow/src/main.rs; then
    echo "Phase 2 runtime ownership marker is missing: $marker" >&2
    exit 1
  fi
done

if rg -n 'axum::serve|TcpListener::bind|timeout_at\(deadline, &mut tasks\)|LifecycleRegistry::default' \
  apps/light-workflow/src/main.rs apps/light-workflow/src/rule_api.rs; then
  echo "Phase 2 forbids a workflow-owned listener or independent shutdown tree." >&2
  exit 1
fi

for route in '/health' '/ready'; do
  if ! rg -Fq ".route(\"$route\"" apps/light-workflow/src/rule_api.rs; then
    echo "Phase 2 control route is missing: $route" >&2
    exit 1
  fi
done

for compose_file in \
  "$workspace_root/portal-config-dev/docker-compose.yml" \
  "$workspace_root/portal-config-loc/all-in-lt/docker-compose-rust.yml" \
  "$workspace_root/light-portal-install/docker-compose.yml"; do
  if ! awk '
    /^  light-workflow:$/ {
      in_workflow = 1
      next
    }
    in_workflow && /^  [A-Za-z0-9_-]+:$/ {
      exit
    }
    in_workflow && /^    healthcheck:$/ {
      in_healthcheck = 1
      next
    }
    in_workflow && in_healthcheck && /^    [A-Za-z0-9_-]+:/ {
      in_healthcheck = 0
    }
    in_workflow && in_healthcheck && index($0, "curl -f http://localhost:8436/ready || exit 1") {
      found = 1
    }
    END {
      exit(found ? 0 : 1)
    }
  ' "$compose_file"; then
    echo "Light Workflow readiness healthcheck is missing from $compose_file" >&2
    exit 1
  fi
done

docker compose -f "$workspace_root/portal-config-dev/docker-compose.yml" config --quiet
docker compose \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose.yml" \
  -f "$workspace_root/portal-config-loc/all-in-lt/docker-compose-rust.yml" \
  config --quiet
docker compose -f "$workspace_root/light-portal-install/docker-compose.yml" config --quiet

echo "Light Workflow Config Server/controller Phase 2 gate passed."
