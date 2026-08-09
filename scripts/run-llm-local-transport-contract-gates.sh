#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
portal_root="${PORTAL_ROOT:-$repo_root/../light-portal}"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
canonical="$repo_root/crates/llm-gateway/tests/fixtures/projection/v3"
mirror="$portal_root/db-provider/src/test/resources/contracts/llm/v3"
mkdir -p "$report_dir"

test -d "$mirror"
for fixture in projection-v3.schema.json projection-acknowledgement-v2.schema.json replica-inventory.schema.json sidecar-manifest.schema.json cutover-artifact-manifest.schema.json signing-golden-vector.json; do
  jq empty "$canonical/$fixture"
  cmp "$canonical/$fixture" "$mirror/$fixture"
done

(cd "$repo_root" && cargo test --locked -p llm-gateway --test contracts_v3)
(cd "$repo_root" && cargo test --locked -p model-provider conformance::signing)
(cd "$repo_root" && cargo test --locked -p light-gateway projection_ack)

jq -n \
  --arg gate "LMT-G0" \
  --arg status "PASS" \
  --arg lightFabricRevision "$(git -C "$repo_root" rev-parse HEAD)" \
  --arg portalRevision "$(git -C "$portal_root" rev-parse HEAD)" \
  --arg fixtureDigest "$(find "$canonical" -maxdepth 1 -type f -name '*.json' -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevisions:{lightFabric:$lightFabricRevision,lightPortal:$portalRevision},canonicalFixtureSetSha256:$fixtureDigest}' \
  > "$report_dir/lmt-g0-contract.json"
echo "LMT-G0 PASS: $report_dir/lmt-g0-contract.json"

