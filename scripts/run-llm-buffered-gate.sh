#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

./scripts/run-llm-request-path-invariants.sh
./scripts/run-llm-body-contract-gate.sh
./scripts/run-llm-accounting-circuit-replay-gate.sh
cargo test --locked -p llm-gateway
cargo check --locked -p light-gateway

echo "[llm-buffered] PASS"

