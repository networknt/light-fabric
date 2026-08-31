#!/usr/bin/env bash
set -euo pipefail

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

for repo in light-fabric light-portal light-portal-event genai-command portal-view; do
  git -C "$workspace_root/$repo" diff --check
done

(
  cd "$fabric_root"
  cargo fmt --all -- --check
  cargo test -p a2a-core -p a2a-client -p light-a2a -p light-agent
  cargo test -p light-workflow --lib -- \
    --skip fixed_action::tests::provider_receives_bounded_typed_idempotent_request \
    --skip fixed_action::tests::uncertain_dispatch_is_unknown_and_status_is_reconciled_by_key
  cargo test -p light-workflow --test config_server_controller_phase0
  cargo test -p light-pingora a2a::tests::
  cargo check --workspace
  grep -q 'a2aOutbound:' apps/light-agent/config/agent.yml
  grep -q 'authorize_forwarded_outbound' frameworks/light-pingora/src/a2a.rs
  grep -q 'OutboundInvocationConstraints' crates/a2a-core/src/lib.rs
  grep -q 'server-owned A2A credential' crates/a2a-client/src/lib.rs
  grep -q 'WORKFLOW_A2A_RAW_DESTINATION_FORBIDDEN' apps/light-workflow/src/executor.rs
  grep -q 'bind_remote_task' crates/a2a-store/src/lib.rs
  grep -q 'managed remote artifacts must be inline' apps/light-a2a/src/lib.rs
  grep -q 'workflow.a2a.bindings' apps/light-workflow/config/workflow.yml
)

for deployment in \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/light-portal-install"; do
  bundle="$deployment/postgres-db/operations/bundle"
  (cd "$bundle" && sha256sum -c bundle.sha256 >/dev/null)
  cmp "$fabric_root/crates/workflow-store/migrations/workflow-postgres/0002_governed_a2a_outbound.sql" \
    "$bundle/crates/workflow-store/migrations/workflow-postgres/0002_governed_a2a_outbound.sql"
  cmp "$fabric_root/crates/workflow-store/migrations/workflow-postgres/0003_governed_a2a_outbound_policy.sql" \
    "$bundle/crates/workflow-store/migrations/workflow-postgres/0003_governed_a2a_outbound_policy.sql"
  cmp "$fabric_root/crates/workflow-store/migrations/workflow-postgres/0004_workflow_consumer_offsets.sql" \
    "$bundle/crates/workflow-store/migrations/workflow-postgres/0004_workflow_consumer_offsets.sql"
done

(
  cd "$workspace_root/light-portal"
  mvn -q -pl db-provider -am -DskipTests compile
  mvn -q -pl db-provider -am -Dtest=A2aPublicationSupportTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
)

(
  cd "$workspace_root/genai-command"
  mvn -q -DskipTests package
)

(
  cd "$workspace_root/portal-view"
  npm run build
)

(
  cd "$workspace_root/light-portal-event"
  jq -e '
    length==4
    and ([.[].data.propertyName] | sort
      == ["a2a.bindings","a2aOutbound.authorizationContextKeyFile","a2aOutbound.bindings","a2aOutbound.enabled"])
    and (all(.[]; .type=="ConfigPropertyCreatedEvent" and .nonce=="0"
      and .aggregateversion==1 and .data.aggregateVersion==0
      and .data.newAggregateVersion==1))
    and ((map(.id)+map(.subject)|unique|length)==8)
  ' genai/20260830-a2a-phase5-config-properties.json >/dev/null
)

echo "A2A Phase 5 static gates PASS"
