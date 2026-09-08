#!/usr/bin/env bash
set -euo pipefail
fabric_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
portal_root="${LIGHT_PORTAL_SOURCE_DIR:-$(dirname "$fabric_root")/light-portal}"
: "${LLM_AUDIT_TEST_DATABASE_URL:?Set a disposable dedicated PostgreSQL audit database URL}"
for command in cargo mvn psql python3 mdbook; do
  command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
gate_dir="$(mktemp -d)"
trap 'rm -rf "$gate_dir"' EXIT
cd "$portal_root"
mvn -q -pl db-provider -am \
  -Dtest=AgentGatewayProjectionTest,AgentGatewayDelegationTest,AgentGatewayPublicationContractTest,AgentPolicyProjectionCompilerTest,LlmModelPersistenceImplContractTest \
  -Dsurefire.failIfNoSpecifiedTests=false \
  "-Dagent.gateway.projection.fixture=$gate_dir/bindings.json" test
cd "$fabric_root"
python3 - "$gate_dir/bindings.json" <<'CHECK'
import json, pathlib, sys
expected = pathlib.Path('crates/llm-gateway/tests/fixtures/authorization/gateway-agent-bindings-v1.json')
if json.loads(pathlib.Path(sys.argv[1]).read_text()) != json.loads(expected.read_text()):
    raise SystemExit('Java gateway binding projection drift')
CHECK
for pass in 1 2; do
  for schema in crates/llm-gateway/migrations/audit-postgres/*.sql; do
    psql "$LLM_AUDIT_TEST_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$schema"
  done
done
cargo test --locked -p llm-gateway --lib
cargo test --locked -p llm-gateway --test local_data_plane
cargo test --locked -p llm-gateway --test agent_alias_publication
cargo test --locked -p llm-gateway audit::tests::postgres_sink_duplicate_delivery_is_idempotent_when_database_is_available --lib -- --ignored
cargo test --locked -p light-gateway llm_
cargo test --locked -p light-gateway dual_token_live_gateway_and_postgres_audit -- --ignored
mdbook build docs
git diff --check
git -C "$portal_root" diff --check
echo 'Agent LLM Phase 2 gates passed, including live Pingora and PostgreSQL audit.'
