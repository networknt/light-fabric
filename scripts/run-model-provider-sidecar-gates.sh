#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
generated="$(mktemp -d)"
trap 'rm -rf -- "$generated"' EXIT
mkdir -p "$report_dir"

(cd "$repo_root" && cargo test --locked -p light-gateway model_provider_sidecar)
(cd "$repo_root" && cargo run --locked -p light-gateway --bin model-provider-sidecar-profile -- \
  apps/light-gateway/k8s/model-provider-sidecar/profile-request.json "$generated")
jq empty "$generated/sidecar-manifest.json"
grep -q 'sidecar-deny' "$generated/handler.yml"
grep -q 'sidecar-identity' "$generated/handler.yml"
if grep -Eq 'api/(pull|delete|tags)' "$generated/handler.yml"; then
  echo "generated sidecar exposes a runtime management path" >&2
  exit 1
fi

jq -n --arg gate "LMT-G3" --arg status "PASS" --arg revision "$(git -C "$repo_root" rev-parse HEAD)" \
  --arg configDigest "$(jq -r .configSha256 "$generated/sidecar-manifest.json")" \
  '{schemaVersion:"1",gate:$gate,status:$status,sourceRevision:$revision,generatedConfigSha256:$configDigest,assertions:["exact_method_path_allowlist","terminal_default_deny","zero_runtime_connections_for_denied_requests","allowed_buffered_and_chunked_proxying","downstream_disconnect_propagation","generated_request_body_limit","generated_response_body_limit","fixed_health_response","bounded_identity_manifest","external_credential_removal","runtime_credential_injection","loopback_runtime","runtime_credential_process_separation","unknown_handlers_fail_boot"]}' \
  > "$report_dir/lmt-g3-sidecar.json"
echo "LMT-G3 PASS: $report_dir/lmt-g3-sidecar.json"
