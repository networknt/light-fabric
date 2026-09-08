#!/usr/bin/env bash
set -euo pipefail
fabric_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
issuer_root="${PORTAL_SERVICE_SOURCE_DIR:-$(dirname "$fabric_root")/portal-service}"
mode="${1:-implementation}"
case "$mode" in implementation|live-gateway) ;; *) echo 'Use implementation or live-gateway' >&2; exit 2 ;; esac
: "${LIGHT_OAUTH_TEST_DATABASE_URL:?Set a disposable PostgreSQL database for the isolated issuer grant test}"
cd "$issuer_root"
cargo test --locked -p light-oauth --bin light-oauth
cargo test --locked -p light-oauth --bin light-oauth registered_workload_grant_signs_identity_and_ignores_form_claims -- --ignored
cd "$fabric_root"
cargo test --locked -p llm-gateway --test local_data_plane
python3 -B -m unittest discover -s scripts/agent-llm -p 'test_*.py'
mdbook build docs
git diff --check
git -C "$issuer_root" diff --check
if [[ "$mode" == live-gateway ]]; then
  : "${AGENT_LLM_QUALIFICATION_CONFIG:?Set the public gateway expectations JSON path}"
  : "${AGENT_LLM_USER_TOKEN_FILE:?Set the owner-only original user JWT file path}"
  : "${AGENT_LLM_WORKLOAD_TOKEN_FILE:?Set the owner-only issued workload JWT file path}"
  : "${AGENT_LLM_CA_FILE:?Set the trusted gateway CA bundle path}"
  : "${AGENT_LLM_REPORT:?Set a report path}"
  # Configure libpq PGHOST/PGDATABASE/PGUSER/PGPASSFILE for the audit database.
  python3 -B scripts/agent-llm/qualify_gateway.py \
    --config "$AGENT_LLM_QUALIFICATION_CONFIG" \
    --user-token-file "$AGENT_LLM_USER_TOKEN_FILE" \
    --workload-token-file "$AGENT_LLM_WORKLOAD_TOKEN_FILE" \
    --ca-file "$AGENT_LLM_CA_FILE" --report "$AGENT_LLM_REPORT"
else
  echo 'IMPLEMENTATION_CHECKS_PASSED. Live rollout and browser qualification NOT RUN.'
fi
