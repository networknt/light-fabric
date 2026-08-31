#!/usr/bin/env bash
set -euo pipefail

fabric_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd -- "$fabric_root/.." && pwd)"

for repo in light-fabric light-portal genai-command portal-view portal-db \
  portal-config-loc portal-config-dev light-portal-install; do
  git -C "$workspace_root/$repo" diff --check
done

(
  cd "$fabric_root"
  cargo fmt --all -- --check
  cargo test -p a2a-protocol -p a2a-client -p a2a-store -p light-a2a
  cargo check --workspace
  jq -e '
    .profileId != ""
    and (.extendedCard.authorizationPolicyDigest | test("^sha256:[0-9a-f]{64}$"))
    and (.dataExtensions | length > 0)
    and (all(.dataExtensions[];
      (.extensionUri | startswith("https://"))
      and (.schemaDigest | test("^sha256:[0-9a-f]{64}$"))
      and (.allowedOperations | length > 0)))
    and (.pushNotifications.registrations | length > 0)
    and (all(.pushNotifications.registrations[];
      (.url | startswith("https://"))
      and (.hmacKeyFile | startswith("/run/secrets/"))))
  ' contracts/a2a/phase6/optional-profile.json >/dev/null
  grep -q 'GetExtendedAgentCard' crates/a2a-protocol/src/lib.rs
  grep -q 'CreateTaskPushNotificationConfig' crates/a2a-protocol/src/lib.rs
  grep -q 'post_signed_callback' crates/a2a-client/src/lib.rs
  grep -q 'claim_push_deliveries' crates/a2a-store/src/lib.rs
  grep -q 'spawn_push_worker' apps/light-a2a/src/main.rs
  grep -q 'required extensions need a later independently qualified profile' \
    apps/light-a2a/src/lib.rs
  (cd docs && mdbook build)
)

source_bundle="$fabric_root/crates/operational-store/release/bundle"
test "$(jq -r .bundleVersion "$source_bundle/manifest.json")" = "1.9.0"
(
  cd "$source_bundle"
  sha256sum -c bundle.sha256 >/dev/null
)

for deployment in \
  "$workspace_root/portal-config-loc/all-in-lt" \
  "$workspace_root/portal-config-dev" \
  "$workspace_root/light-portal-install"; do
  bundle="$deployment/postgres-db/operations/bundle"
  (cd "$bundle" && sha256sum -c bundle.sha256 >/dev/null)
  cmp "$source_bundle/manifest.json" "$bundle/manifest.json"
  cmp "$source_bundle/migration-order.tsv" "$bundle/migration-order.tsv"
  cmp "$fabric_root/crates/a2a-store/migrations/a2a-postgres/0003_governed_push_delivery.sql" \
    "$bundle/crates/a2a-store/migrations/a2a-postgres/0003_governed_push_delivery.sql"
  grep -q 'OPERATIONAL_BUNDLE_VERSION: 1.9.0' "$deployment/docker-compose.yml"
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
  mvn -q -Dtest=A2aAuthoringCommandTest test
)

(
  cd "$workspace_root/portal-view"
  npm run build
)

grep -q 'a2a_extended_card_profile_t' "$workspace_root/portal-db/postgres/ddl.sql"
grep -q 'a2a_push_profile_t' "$workspace_root/portal-db/postgres/ddl.sql"
grep -q 'a2a_callback_registration_t' "$workspace_root/portal-db/postgres/ddl.sql"

echo "A2A Phase 6 static gates PASS"
