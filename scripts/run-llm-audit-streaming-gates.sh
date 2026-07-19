#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
database_url="${1:-${LLM_AUDIT_TEST_DATABASE_URL:-}}"

"$repo_root/scripts/run-llm-audit-wal-gate.sh" "$database_url"

echo "[llm-audit-streaming] LF-9 streaming semantics and abuse bounds"
(cd "$repo_root" && cargo test --locked -p model-provider \
  codec::tests::decodes_the_same_events_at_every_chunk_split)
(cd "$repo_root" && cargo test --locked -p model-provider \
  providers::openai::codec::tests::preserves_tool_argument_fragments_across_chunks)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane full_sse_)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane \
  early_sse_never_emits_done_or_retries_after_visible_output_error)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane \
  early_sse_disconnect_cancels_upstream_and_releases_stream_permits)
(cd "$repo_root" && cargo test --locked -p llm-gateway --test local_data_plane \
  early_sse_deadline_cancels_a_trickling_provider_and_releases_permits)
(cd "$repo_root" && cargo test --locked -p light-gateway llm_sse_smoke_streams_openai_frames_over_live_pingora)
(cd "$repo_root" && cargo test --locked -p light-pingora downstream_drain_rate_enforces_grace_and_threshold)

echo "[llm-audit-streaming] PERF-2 manifest and runner contract"
jq empty "$repo_root/benchmarks/llm-gateway/manifests/perf2-manifest.json"
jq empty "$repo_root/benchmarks/llm-gateway/manifests/perf2-environment.example.json"
bash -n "$repo_root/benchmarks/llm-gateway/scripts/run-perf2-candidate.sh"

echo "[llm-audit-streaming] PASS"
