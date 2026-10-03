#!/usr/bin/env bash
# Owner-run readiness one-shot. No evaluator advertisement or prefix fallback.
set -euo pipefail
umask 077
fail() { echo 'W7_CONTROLLER_PAGE_REFUSED' >&2; exit 2; }
operations_root="${W7_OPERATIONS_ROOT:-/opt/operational-store}"
bash "$operations_root/bin/w7-startup-guard.sh" "$operations_root" || fail
header_file="${W7_CONTROLLER_AUTH_HEADER_FILE:-/run/secrets/w7-controller-page-authorization}"
[[ -s "$header_file" ]] || fail
[[ "$(wc -l < "$header_file")" == 1 ]] || fail
grep -Eq '^Authorization: Bearer [A-Za-z0-9._-]+$' "$header_file" || fail
response="$(mktemp)"
trap 'rm -f -- "$response"' EXIT
# All curl stderr and body stay private; no authorization or result data in logs.
status="$(curl --silent --fail --connect-timeout 10 --max-time 30 \
  --cacert /config/ca.pem --header "@$header_file" \
  --output "$response" --write-out '%{http_code}' \
  'https://controller:8438/internal/execution/results/page?limit=1' 2>/dev/null)" || fail
[[ "$status" == 200 ]] || fail
grep -Eq '"items"[[:space:]]*:[[:space:]]*\[' "$response" || fail
grep -Eq '"nextCursor"[[:space:]]*:' "$response" || fail
echo 'W7_CONTROLLER_PAGED_API_READY'
