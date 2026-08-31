#!/usr/bin/env bash
set -euo pipefail

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

"$fabric_root/scripts/run-a2a-phase6-gates.sh"

jq -e '
  .schemaVersion == 1
  and .profileId == "a2a-production-qualification-v1"
  and .releaseDecision == "NOT_QUALIFIED"
  and .identity.required == ["host","serviceId","envTag"]
  and (.identity.forbidden | index("instanceId") != null)
  and .immutableEvidence.operationalBundleVersion == "1.9.0"
  and .immutableEvidence.rollbackGenerations == 1
  and .environmentEvidence.minimumCanaryHours == 24
  and (.automatedGates | index("push-lease-takeover") != null)
  and (.automatedGates | index("expired-projection-readiness") != null)
' "$fabric_root/contracts/a2a/phase7/qualification-contract.json" >/dev/null

jq -e '.releaseDecision == "NOT_QUALIFIED" and .releaseVersion == 0
  and (.approvals | length == 0)' \
  "$fabric_root/contracts/a2a/phase7/evidence-template.json" >/dev/null

for deployment in \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/light-portal-install"; do
  for module in a2a.yml startup.yml portal-registry.yml server.yml client.yml security.yml; do
    cmp "$fabric_root/apps/light-a2a/config/$module" \
      "$deployment/light-a2a-rust/config/$module"
  done
  values="$deployment/light-a2a-rust/config/values.yml"
  ! grep -qE '^(runtimePolicy\.|a2aPolicy\.bindings:)' "$values"
  grep -q '^startup.host:' "$values"
  grep -q '^server.serviceId:' "$values"
  grep -q '^server.environment:' "$values"
  grep -q '^artifactStore.databaseUrlFile: /run/secrets/artifact-database-url$' "$values"
  grep -q '/run/secrets/artifact-database-url:ro' "$deployment/docker-compose.yml"
  grep -q 'A2A_SIGNING_SERVICE_URL:.*light-oauth:6881' "$deployment/docker-compose.yml"
  grep -q '/run/secrets/a2a-signing:ro' "$deployment/docker-compose.yml"
  grep -q '/var/lib/light-agent/a2a-artifacts' "$deployment/docker-compose.yml"
  grep -q '^a2a_signing_key_root: "/run/secrets/a2a-signing"$' \
    "$deployment/light-oauth-rust/config/values.yml"
  grep -q 'http://localhost:8448/_a2a/ready' "$deployment/docker-compose.yml"
  docker compose -f "$deployment/docker-compose.yml" --profile a2a config --quiet
done

BUSINESS_AGENT_IMAGE=example/business-agent:qualification \
  docker compose -f "$fabric_root/apps/light-a2a/deploy/sidecar/docker-compose.yml" config --quiet
docker compose -f "$fabric_root/apps/light-a2a/deploy/shared/docker-compose.yml" config --quiet

grep -q 'claim_push_deliveries(self.host_id, worker_id, 1, lease_seconds)' \
  "$fabric_root/apps/light-a2a/src/lib.rs"
grep -q 'push callback timeout must leave five seconds' \
  "$workspace_root/genai-command/src/main/java/net/lightapi/portal/genai/command/handler/PutA2aAuthoring.java"
grep -q 'request_timeout_ms + 5000 <= lease_seconds \* 1000' \
  "$workspace_root/portal-db/postgres/ddl.sql"
grep -q 'serde_json_canonicalizer::to_vec' \
  "$fabric_root/crates/a2a-protocol/src/lib.rs"
grep -q 'contentDigest does not match canonical bindings' \
  "$fabric_root/apps/light-a2a/src/lib.rs"
grep -q 'spawn_artifact_retention_worker' \
  "$fabric_root/apps/light-a2a/src/main.rs"

echo "A2A Phase 7 source, deployment, readiness, and qualification gates PASS"
