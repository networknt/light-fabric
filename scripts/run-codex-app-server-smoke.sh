#!/usr/bin/env bash
set -euo pipefail

codex_executable="${LIGHT_CODEX_SMOKE_EXECUTABLE:-$(command -v codex || true)}"
smoke_profile="${LIGHT_CODEX_SMOKE_PROFILE:-protocol}"
audit_database_url="${LIGHT_CODEX_SMOKE_AUDIT_DATABASE_URL:-}"
if [[ "$smoke_profile" != "protocol" && "$smoke_profile" != "personal-subscription" ]]; then
  echo "LIGHT_CODEX_SMOKE_PROFILE must be protocol or personal-subscription" >&2
  exit 1
fi
if [[ -z "$codex_executable" || ! -x "$codex_executable" ]]; then
  echo "pinned Codex executable is required for the App Server qualification smoke test" >&2
  exit 1
fi
if [[ "$($codex_executable --version)" != "codex-cli 0.153.4" ]]; then
  echo "Codex App Server smoke test found an unqualified version" >&2
  exit 1
fi

stderr_file="$(mktemp)"
workspace="$(mktemp -d)"
remove_smoke_home=false
if [[ "$smoke_profile" == "personal-subscription" ]]; then
  smoke_home="${CODEX_HOME:-$HOME/.codex}"
  if [[ ! -d "$smoke_home" ]]; then
    echo "personal-subscription smoke requires an existing CODEX_HOME" >&2
    rm -f "$stderr_file"
    rm -rf "$workspace"
    exit 1
  fi
else
  smoke_home="$(mktemp -d)"
  remove_smoke_home=true
fi
cleanup() {
  if [[ -n "${CODEX_PID:-}" ]]; then
    kill "$CODEX_PID" 2>/dev/null || true
    wait "$CODEX_PID" 2>/dev/null || true
  fi
  rm -f "$stderr_file"
  rm -rf "$workspace"
  if [[ "$remove_smoke_home" == "true" ]]; then
    rm -rf "$smoke_home"
  fi
}
trap cleanup EXIT

if [[ "$smoke_profile" == "personal-subscription" ]]; then
  expected_codex_digest="56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da"
  if [[ "$(sha256sum "$codex_executable" | cut -d' ' -f1)" != "$expected_codex_digest" ]]; then
    echo "personal-subscription smoke requires the qualified native Codex binary, not an npm launcher" >&2
    exit 1
  fi
fi

audit_count() {
  psql "$audit_database_url" -v ON_ERROR_STOP=1 -Atc \
    "SELECT count(*) FROM llm_audit_event_t" 2>/dev/null
}

before_audit_count=""
if [[ -n "$audit_database_url" ]]; then
  command -v psql >/dev/null || {
    echo "psql is required when LIGHT_CODEX_SMOKE_AUDIT_DATABASE_URL is set" >&2
    exit 1
  }
  before_audit_count="$(audit_count)"
fi

coproc CODEX { CODEX_HOME="$smoke_home" "$codex_executable" app-server 2>"$stderr_file"; }
printf '%s\n' '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"light-qualification","title":"Light qualification","version":"1"},"capabilities":{"experimentalApi":false,"requestAttestation":false}}}' >&"${CODEX[1]}"

response=""
for _ in $(seq 1 20); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e '.id == 1 and .result != null and .error == null' <<<"$line" >/dev/null; then
      response="$line"
      break
    fi
  fi
done
if [[ -z "$response" ]]; then
  echo "Codex App Server did not complete initialize" >&2
  sed -n '1,20p' "$stderr_file" >&2
  exit 1
fi

printf '%s\n' '{"method":"initialized"}' >&"${CODEX[1]}"
printf '%s\n' '{"id":2,"method":"account/read","params":{"refreshToken":false}}' >&"${CODEX[1]}"
account_response=""
for _ in $(seq 1 20); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e '.id == 2 and .result.requiresOpenaiAuth != null and .error == null' <<<"$line" >/dev/null; then
      account_response="$line"
      break
    fi
  fi
done
if [[ -z "$account_response" ]]; then
  echo "Codex App Server did not complete account/read" >&2
  exit 1
fi
if [[ "$smoke_profile" == "protocol" ]]; then
  echo "Pinned Codex App Server initialize/account lifecycle passed."
  exit 0
fi
if ! jq -e '.result.requiresOpenaiAuth == true and .result.account.type == "chatgpt"' \
  <<<"$account_response" >/dev/null; then
  echo "personal-subscription smoke requires an authenticated ChatGPT Codex account" >&2
  exit 1
fi

thread_request="$(jq -nc --arg cwd "$workspace" --arg model "${LIGHT_CODEX_SMOKE_MODEL:-}" '{id:3,method:"thread/start",params:({cwd:$cwd,approvalPolicy:"never",sandbox:"read-only",serviceName:"light-subscription-smoke",ephemeral:true} + (if $model == "" then {} else {model:$model} end))}')"
printf '%s\n' "$thread_request" >&"${CODEX[1]}"
thread_id=""
for _ in $(seq 1 30); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e '.id == 3 and .result.thread.id != null and .error == null' <<<"$line" >/dev/null; then
      thread_id="$(jq -r '.result.thread.id' <<<"$line")"
      selected_model="$(jq -r '.result.model' <<<"$line")"
      if [[ -n "${LIGHT_CODEX_SMOKE_MODEL:-}" && "$selected_model" != "$LIGHT_CODEX_SMOKE_MODEL" ]]; then
        echo "Codex selected $selected_model instead of the requested qualification model" >&2
        exit 1
      fi
      break
    fi
  fi
done
if [[ -z "$thread_id" ]]; then
  echo "Codex App Server did not start the personal-subscription thread" >&2
  exit 1
fi

turn_request="$(jq -nc --arg thread "$thread_id" --arg cwd "$workspace" '{id:4,method:"turn/start",params:{threadId:$thread,input:[{type:"text",text:"Reply with exactly: codex subscription path passed",text_elements:[]}],cwd:$cwd,approvalPolicy:"never"}}')"
printf '%s\n' "$turn_request" >&"${CODEX[1]}"
turn_id=""
for _ in $(seq 1 30); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e '.id == 4 and .result.turn.id != null and .error == null' <<<"$line" >/dev/null; then
      turn_id="$(jq -r '.result.turn.id' <<<"$line")"
      break
    fi
  fi
done
if [[ -z "$turn_id" ]]; then
  echo "Codex App Server did not start the personal-subscription turn" >&2
  exit 1
fi

final_message=""
turn_status=""
for _ in $(seq 1 180); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e --arg thread "$thread_id" --arg turn "$turn_id" \
      '.method == "item/completed" and .params.threadId == $thread and .params.turnId == $turn and .params.item.type == "agentMessage"' \
      <<<"$line" >/dev/null; then
      final_message="$(jq -r '.params.item.text' <<<"$line")"
    fi
    if jq -e --arg turn "$turn_id" \
      '.method == "turn/completed" and .params.turn.id == $turn' <<<"$line" >/dev/null; then
      turn_status="$(jq -r '.params.turn.status' <<<"$line")"
      break
    fi
  fi
done
if [[ "$turn_status" != "completed" || "$final_message" != "codex subscription path passed" ]]; then
  echo "personal-subscription turn failed: status=${turn_status:-missing} message=${final_message:-missing}" >&2
  exit 1
fi
if [[ -n "$audit_database_url" ]]; then
  after_audit_count="$(audit_count)"
  if [[ "$after_audit_count" != "$before_audit_count" ]]; then
    echo "direct subscription smoke unexpectedly changed the llm-gateway audit row count" >&2
    exit 1
  fi
fi
echo "Pinned Codex App Server personal-subscription turn passed without llm-gateway routing."
echo "Qualified model: $selected_model"
