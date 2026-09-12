#!/usr/bin/env bash
# Build the native workers and local Claude Agent image. Build Java publishers
# with their normal build.sh/release pipeline and select the resulting images
# through PORTAL_HYBRID_COMMAND_IMAGE and PORTAL_HYBRID_QUERY_IMAGE.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root" && cargo build --locked -p light-agent-worker --features claude-prototype --bin light-claude-worker && cargo build --locked -p light-workflow-runner --bin light-workflow-runner && cargo build --locked -p light-agent --bin light-agent --release --target x86_64-unknown-linux-musl)
(cd "$root" && docker build -f apps/light-agent/docker/Dockerfile.local-claude -t networknt/light-agent:claude-phase3-local target/x86_64-unknown-linux-musl/release)
printf '%s\n' 'Images built. Regenerate runner configuration and republish its profile if worker/template digests changed.'
