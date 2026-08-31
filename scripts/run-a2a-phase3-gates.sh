#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 postgresql://.../DISPOSABLE_DATABASE" >&2
  exit 2
fi

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

for repo in portal-db light-portal genai-command genai-query portal-view light-fabric light-portal-event portal-config-loc portal-config-dev light-portal-install; do
  git -C "$workspace_root/$repo" diff --check
done

bash "$workspace_root/portal-db/postgres/tests/run-a2a-phase3-backend-transport-schema-gate.sh" "$1"

(
  cd "$fabric_root"
  cargo fmt --all -- --check
  cargo test -p a2a-backend -p a2a-store -p artifact-store -p light-a2a
  cargo build -p light-a2a
  tck_report_dir="$(mktemp -d)"
  trap 'rm -rf -- "$tck_report_dir"' EXIT
  export A2A_TCK_REPORT_DIR="$tck_report_dir"
  export LIGHT_A2A_BUILD_SHA256="$(sha256sum target/debug/light-a2a | cut -d' ' -f1)"
  PYTHONPATH=sdks/a2a-backend/python python3 -m unittest discover -s sdks/a2a-backend/python/tests
  npm --prefix sdks/a2a-backend/typescript test
  mvn -q -f sdks/a2a-backend/java/pom.xml test
  python3 contracts/a2a-backend/v1/tck/verify_reports.py "$tck_report_dir"
  python3 -m json.tool contracts/a2a-backend/v1/tck/cases.json >/dev/null
  (cd contracts/a2a-backend/v1 && sha256sum -c openapi.sha256)
  for schema in contracts/a2a-backend/v1/schemas/*.json contracts/a2a-backend/v1/fixtures/*.json; do
    python3 -m json.tool "$schema" >/dev/null
  done
  grep -q 'POST /v1/invoke-stream' docs/src/product/light-gateway/a2a-gateway.md
  grep -q 'net.lightapi.backend-contract: light-a2a-backend-v1' apps/light-a2a/deploy/sidecar/docker-compose.yml
)

for deployment in \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/light-portal-install"; do
  bundle="$deployment/postgres-db/operations/bundle"
  (cd "$bundle" && sha256sum -c bundle.sha256 >/dev/null)
  cmp "$fabric_root/crates/a2a-store/migrations/a2a-postgres/0001_external_a2a_durability.sql" \
    "$bundle/crates/a2a-store/migrations/a2a-postgres/0001_external_a2a_durability.sql"
  cmp "$fabric_root/crates/a2a-store/migrations/a2a-postgres/0002_backend_skill_correlation.sql" \
    "$bundle/crates/a2a-store/migrations/a2a-postgres/0002_backend_skill_correlation.sql"
  # Later phases extend the same immutable bundle. Phase 3 must keep proving
  # its migrations are present without pinning the historical intermediate
  # bundle number and making the final gate impossible to rerun.
  test "$(jq -r .bundleVersion "$bundle/manifest.json")" = "1.9.0"
done

(
  cd "$workspace_root/light-portal"
  mvn -q -pl db-provider -am -DskipTests compile
  mvn -q -pl db-provider -am -Dtest=A2aPublicationSupportTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
)
(
  cd "$workspace_root/genai-command"
  mvn -q -Dtest=A2aAuthoringCommandTest test
)
(
  cd "$workspace_root/genai-query"
  mvn -q -DskipTests package
)
(
  cd "$workspace_root/portal-view"
  npm run build
)
(
  cd "$workspace_root/light-portal-event"
  jq -e '
    length==7
    and (all(.[]; .type=="ConfigPropertyCreatedEvent" and .nonce=="0"
      and .aggregateversion==1 and .data.aggregateVersion==0
      and .data.newAggregateVersion==1 and (.data.propertyName|startswith("artifactStore."))))
    and ((map(.id)+map(.subject)|unique|length)==14)
  ' genai/20260830-a2a-phase3-config-properties.json >/dev/null
)

echo "A2A Phase 3 gates PASS"
