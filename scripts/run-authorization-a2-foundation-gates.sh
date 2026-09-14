#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# These are bounded source/foundation gates, not the A2 selected-stack exit gate.
# No scheduled or hours-long A1 renewal run is started here.
cargo test -p workflow-action --test contract
cargo test -p workflow-invocation-contract
cargo test -p light-security --lib
cargo test -p light-axum --lib mtls
cargo test -p light-workflow --lib
cargo test -p light-agent --lib
cargo test -p light-agent --bin light-agent
cargo test -p light-knowledge --lib
cargo test -p pingora-core --lib --features rustls request_write_guard
cargo test -p light-pingora --lib
cargo test -p light-gateway --lib
cargo test -p light-gateway --bin light-gateway
cargo check -p light-workflow -p light-agent -p light-knowledge -p light-gateway
(cd crates/operational-store/release/bundle && sha256sum --check bundle.sha256)
if [[ -n "${WORKFLOW_ACTION_TEST_DATABASE_URL:-}" ]]; then
    # Requires an EMPTY disposable database; the test creates workflow_ops.
    cargo test -p workflow-action --test postgres -- --ignored
else
    echo 'PostgreSQL action gate NOT RUN: set WORKFLOW_ACTION_TEST_DATABASE_URL to an empty disposable database.'
fi
printf '%s\n' 'Foundation checks finished. This does not qualify A2 route integration or deployment.'
