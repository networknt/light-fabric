#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SSE Phase 3 resource qualification requires Linux /proc metrics." >&2
  exit 2
fi

(
  cd "$repo_root"
  cargo test --locked -p light-gateway sse_phase3_protocol_and_route_matrix
  cargo test --locked -p light-gateway http_1_0_streaming_uses_close_delimited_framing
  cargo test --locked -p light-gateway http_2_upstream_idle_timeout_uses_normal_downstream_failure_path
  cargo test --locked -p light-gateway accept_is_provisional_but_confirmed_stream_rejects_complete_body_filter
  cargo test --locked -p light-gateway proxy_deadlines_are_isolated_between_ordinary_and_expected_stream_requests
  cargo test --locked -p light-gateway sse_phase3_soak_shutdown_and_post_commit_retry_gate -- --ignored --nocapture
)

echo "SSE passthrough Phase 3 gates PASS"
