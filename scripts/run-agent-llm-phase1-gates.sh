#!/usr/bin/env bash
set -euo pipefail
fabric_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
portal_root="${LIGHT_PORTAL_SOURCE_DIR:-$(dirname "$fabric_root")/light-portal}"
for command in mvn cargo mdbook cmp python3; do
  command -v "$command" >/dev/null || { echo "Required command missing: $command" >&2; exit 1; }
done
[[ -f "$portal_root/db-provider/pom.xml" ]] || { echo "Set LIGHT_PORTAL_SOURCE_DIR to the light-portal checkout" >&2; exit 1; }
gate_dir="$(mktemp -d)"
trap 'rm -rf "$gate_dir"' EXIT
cd "$portal_root"
mvn -q -pl db-provider -am \
  -Dtest=AgentGatewayDelegationTest,AgentGatewayPublicationContractTest,AgentPolicyProjectionCompilerTest \
  -Dsurefire.failIfNoSpecifiedTests=false \
  "-Dagent.gateway.fixture=$gate_dir/gateway-projection-v1.json" \
  "-Dagent.alias.fixture=$gate_dir/agent-alias-v1.json" test
cd "$fabric_root"
python3 - "$gate_dir" <<'CHECK'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for generated, checked in [
    (root / "gateway-projection-v1.json", pathlib.Path("apps/light-agent/tests/fixtures/gateway-projection-v1.json")),
    (root / "agent-alias-v1.json", pathlib.Path("crates/llm-gateway/tests/fixtures/agent-alias-v1.json")),
]:
    if json.loads(generated.read_text()) != json.loads(checked.read_text()):
        raise SystemExit(f"Java publication fixture drift: {checked}")
CHECK
cmp "$portal_root/db-provider/src/test/resources/agent-gateway-delegation-v1.json" \
  crates/agent-runtime-protocol/tests/fixtures/gateway-delegation-v1.json
cargo test --locked -p agent-runtime-protocol gateway_delegation
cargo test --locked -p light-agent agent_config::tests
cargo test --locked -p llm-gateway --test agent_alias_publication
cargo test --locked -p llm-gateway --test local_data_plane internal_alias_invocation_is_bound_to_its_approved_principal
cargo test --locked -p llm-gateway --test local_data_plane models_never_enumerate_internal_aliases
mdbook build docs
git diff --check
git -C "$portal_root" diff --check
echo "Agent LLM Phase 1 contract gates passed (mock JDBC; no live activation)."
