#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/run-coding-harness-phase4-gates.sh

cargo test --locked -p coding-agent-runtime --lib \
  optional_adapters_are_fail_closed_until_independently_qualified
cargo test --locked -p light-agent-worker --lib \
  codex_app_server::tests::pinned_contract_rejects_schema_drift

app_evidence="contracts/coding-adapters/codex-app-server-v1-qualification.json"
embedded_evidence="contracts/coding-adapters/codex-embedded-v1-prototype.json"
test "$(sha256sum "$app_evidence" | cut -d' ' -f1)" = \
  "268432fcff0f5d90ad58f45be6d8e433baedcb4c6e96e7b16e4c82ee262ebf4c"
test "$(sha256sum "$embedded_evidence" | cut -d' ' -f1)" = \
  "98fc7e79b0680efa86f534dd456fd89f7959ed59b1b3bd421727f5a05dcf9174"

jq -e '
  .adapterId == "codex-app-server-v1" and
  .status == "qualified" and
  .contractDigestRequired == true and
  (.evidenceGates | index("run-codex-app-server-smoke.sh")) != null and
  (.evaluatedDimensions | length) == 13
' "$app_evidence" >/dev/null
jq -e '
  .adapterId == "codex-embedded-v1" and
  .status == "prototype-only" and
  .productionQualified == false and
  .upstream.revision == "657a993cbee87acf52d14b758ce49dbd46d1b8eb"
' "$embedded_evidence" >/dev/null

# Optional harnesses must not leak into the production worker capability set.
if rg -n 'codex-embedded-v1|claude-code-v1' \
  apps/light-agent-worker/src apps/light-agent/src; then
  echo "an unqualified optional adapter entered a production selection path" >&2
  exit 1
fi

cargo fmt --check --manifest-path prototypes/codex-embedded-v1/Cargo.toml
if [[ "${LIGHT_RUN_CODEX_EMBEDDED_PROBE:-0}" == "1" ]]; then
  CCACHE_DISABLE=1 cargo check --locked \
    --manifest-path prototypes/codex-embedded-v1/Cargo.toml
  CCACHE_DISABLE=1 cargo run --locked --quiet \
    --manifest-path prototypes/codex-embedded-v1/Cargo.toml -- 10000
fi

mdbook build docs
echo "Coding harness Phase 5 gates passed; codex-embedded-v1 remains prototype-only."
