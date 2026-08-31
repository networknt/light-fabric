#!/usr/bin/env bash
set -euo pipefail

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

for repo in light-fabric light-portal light-portal-event portal-config-loc portal-config-dev light-portal-install; do
  git -C "$workspace_root/$repo" diff --check
done

(
  cd "$fabric_root"
  cargo fmt --all -- --check
  cargo test -p a2a-server -p agent-store -p light-agent
  cargo check --workspace
  grep -q 'a2aPolicy.contentDigest' apps/light-agent/config/agent.yml
  grep -q 'a2aPolicy.publicSkills' apps/light-agent/config/agent.yml
  grep -q 'message_id' crates/agent-store/migrations/agent-postgres/0003_native_a2a_phase4.sql
  grep -q 'provenance_digest' crates/agent-store/migrations/agent-postgres/0003_native_a2a_phase4.sql
)

(
  cd "$workspace_root/light-portal"
  mvn -q -pl db-provider -am -DskipTests compile
  mvn -q -pl db-provider -am -Dtest=A2aPublicationSupportTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
)

(
  cd "$workspace_root/light-portal-event"
  jq -e '
    length==2
    and ([.[].data.propertyName] | sort
      == ["a2aPolicy.contentDigest","a2aPolicy.publicSkills"])
    and (all(.[]; .type=="ConfigPropertyCreatedEvent" and .nonce=="0"
      and .aggregateversion==1 and .data.aggregateVersion==0
      and .data.newAggregateVersion==1))
    and ((map(.id)+map(.subject)|unique|length)==4)
  ' genai/20260830-a2a-phase4-config-properties.json >/dev/null
)

for deployment in \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/light-portal-install"; do
  bundle="$deployment/postgres-db/operations/bundle"
  (cd "$bundle" && sha256sum -c bundle.sha256 >/dev/null)
  cmp "$fabric_root/crates/agent-store/migrations/agent-postgres/0003_native_a2a_phase4.sql" \
    "$bundle/crates/agent-store/migrations/agent-postgres/0003_native_a2a_phase4.sql"
  grep -q '"bundleVersion": "1.6.0"' "$bundle/manifest.json"
  grep -q $'^15\tagent-store\tagent_ops\t0003_native_a2a_phase4\t' \
    "$bundle/migration-order.tsv"
  grep -q 'OPERATIONAL_BUNDLE_VERSION: 1.6.0' "$deployment/docker-compose.yml"
done

echo "A2A Phase 4 static gates PASS"
