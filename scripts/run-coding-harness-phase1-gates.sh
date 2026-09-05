#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

schema="contracts/codex-app-server/v0.153.2/json/codex_app_server_protocol.v2.schemas.json"
expected="d3eace08be5dca386bfd1f1e8df650058b4113f1e10870a284d775d75517576a"
actual="$(sha256sum "$schema" | awk '{print $1}')"
test "$actual" = "$expected"
test -f contracts/codex-app-server/v0.153.2/typescript/ClientRequest.ts
test -f contracts/codex-app-server/v0.153.2/typescript/ServerRequest.ts
./scripts/run-codex-app-server-smoke.sh

if rg -n 'light-pi-rpc-adapter|coding\.pi-rpc-v1|PI_RPC_|@earendil-works/pi-coding-agent' \
  Cargo.toml Cargo.lock apps crates scripts --glob '!run-coding-harness-phase1-gates.sh'; then
  echo "retired Pi runtime artifact remains" >&2
  exit 1
fi

cargo test --locked \
  -p agent-runtime-protocol \
  -p coding-agent-runtime \
  -p agent-materializer \
  -p execution-backend-cube \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent --lib
cargo test --locked -p light-agent --bin light-agent coding_profile_accepts_only_pinned_codex_app_server_adapter
cargo clippy --locked \
  -p agent-runtime-protocol \
  -p coding-agent-runtime \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent --all-targets --no-deps
cargo fmt --check \
  -p agent-runtime-protocol \
  -p coding-agent-runtime \
  -p agent-materializer \
  -p execution-backend-cube \
  -p light-agent-worker \
  -p light-workflow-runner \
  -p light-agent
mdbook build docs
