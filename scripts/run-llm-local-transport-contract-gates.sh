#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
canonical="$repo_root/crates/llm-gateway/tests/fixtures/provider/v1"
mkdir -p "$report_dir"

for fixture in sidecar-manifest.schema.json cutover-artifact-manifest.schema.json signing-golden-vector.json; do
  jq empty "$canonical/$fixture"
done

(cd "$repo_root" && cargo test --locked -p llm-gateway --test provider_contracts)
(cd "$repo_root" && cargo test --locked -p model-provider conformance::signing)

jq -n \
  --arg gate "LMT-G0" \
  --arg status "PASS" \
  --arg lightFabricRevision "$(git -C "$repo_root" rev-parse HEAD)" \
  --arg fixtureDigest "$(find "$canonical" -maxdepth 1 -type f -name '*.json' -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevisions:{lightFabric:$lightFabricRevision},canonicalFixtureSetSha256:$fixtureDigest}' \
  > "$report_dir/lmt-g0-contract.json"
echo "LMT-G0 PASS: $report_dir/lmt-g0-contract.json"
