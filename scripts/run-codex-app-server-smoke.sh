#!/usr/bin/env bash
set -euo pipefail

codex_executable="${LIGHT_CODEX_SMOKE_EXECUTABLE:-$(command -v codex || true)}"
if [[ -z "$codex_executable" || ! -x "$codex_executable" ]]; then
  echo "pinned Codex executable is required for the App Server qualification smoke test" >&2
  exit 1
fi
if [[ "$($codex_executable --version)" != "codex-cli 0.153.2" ]]; then
  echo "Codex App Server smoke test found an unqualified version" >&2
  exit 1
fi

stderr_file="$(mktemp)"
smoke_home="$(mktemp -d)"
cleanup() {
  if [[ -n "${CODEX_PID:-}" ]]; then
    kill "$CODEX_PID" 2>/dev/null || true
    wait "$CODEX_PID" 2>/dev/null || true
  fi
  rm -f "$stderr_file"
  rm -rf "$smoke_home"
}
trap cleanup EXIT

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
for _ in $(seq 1 20); do
  if IFS= read -r -t 1 line <&"${CODEX[0]}"; then
    if jq -e '.id == 2 and .result.requiresOpenaiAuth != null and .error == null' <<<"$line" >/dev/null; then
      echo "Pinned Codex App Server initialize/account lifecycle passed."
      exit 0
    fi
  fi
done
echo "Codex App Server did not complete account/read" >&2
exit 1
