#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
output="${LLM_CODEX_EVIDENCE_OUTPUT:-$repo_root/benchmarks/llm-gateway/reports/codex-responses-smoke.json}"
for name in LIGHT_LLM_TOKEN LLM_SDK_BASE_URL LLM_SDK_RESPONSES_MODEL LLM_SDK_REVISION LLM_SDK_PROJECTION_DIGEST LLM_SDK_RESPONSES_CONFORMANCE_DIGEST; do
  if [[ -z "${!name:-}" ]]; then echo "$name is required" >&2; exit 2; fi
done
command -v codex >/dev/null || { echo "codex CLI is required" >&2; exit 2; }
scratch="$(mktemp -d)"
cleanup() { find "$scratch" -depth -delete; }
trap cleanup EXIT
common=(exec --ignore-user-config --ignore-rules --ephemeral --skip-git-repo-check --sandbox read-only --json -m "$LLM_SDK_RESPONSES_MODEL" -c 'model_provider="light_gateway"' -c 'model_providers.light_gateway.name="Light LLM Gateway"' -c "model_providers.light_gateway.base_url=\"$LLM_SDK_BASE_URL\"" -c 'model_providers.light_gateway.wire_api="responses"' -c 'model_providers.light_gateway.env_key="LIGHT_LLM_TOKEN"' -c 'model_providers.light_gateway.request_max_retries=0' -c 'model_providers.light_gateway.stream_max_retries=0')
codex "${common[@]}" "Reply with exactly LIGHT_GATEWAY_CODEX_OK. Do not use tools." >"$scratch/text.jsonl"
codex "${common[@]}" "Use the shell tool to run printf LIGHT_GATEWAY_TOOL_OK, then reply with exactly LIGHT_GATEWAY_CODEX_TOOL_OK." >"$scratch/tool.jsonl"
text_ok="$(jq -s 'any(.[]; .type == "item.completed" and .item.type == "agent_message" and (.item.text | contains("LIGHT_GATEWAY_CODEX_OK")))' "$scratch/text.jsonl")"
tool_call_ok="$(jq -s 'any(.[]; .type == "item.completed" and .item.type == "command_execution")' "$scratch/tool.jsonl")"
tool_result_ok="$(jq -s 'any(.[]; .type == "item.completed" and .item.type == "agent_message" and (.item.text | contains("LIGHT_GATEWAY_CODEX_TOOL_OK")))' "$scratch/tool.jsonl")"
mkdir -p "$(dirname -- "$output")"
jq -n --arg revision "$LLM_SDK_REVISION" --arg generatedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg projectionDigest "$LLM_SDK_PROJECTION_DIGEST" --arg conformanceDigest "$LLM_SDK_RESPONSES_CONFORMANCE_DIGEST" --arg codexVersion "$(codex --version 2>/dev/null)" --argjson text "$text_ok" --argjson toolCall "$tool_call_ok" --argjson toolResult "$tool_result_ok" '{schemaVersion:"1",revision:$revision,generatedAt:$generatedAt,projectionDigest:$projectionDigest,conformanceDigest:$conformanceDigest,codexVersion:$codexVersion,sanitized:true,secretMaterialRecorded:false,operations:{text:$text,clientFunctionLoop:($toolCall and $toolResult)},status:(if ($text and $toolCall and $toolResult) then "pass" else "fail" end)}' >"$output"
echo "wrote $output"
