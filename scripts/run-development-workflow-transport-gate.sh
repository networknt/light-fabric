#!/usr/bin/env bash
set -euo pipefail
# Read-only, live qualification of both Workflow Agent mTLS identities. This
# does not mint a run grant, create a job, or claim native execution coverage.
# Credentials remain inside their existing containers and never enter argv.
for adapter in codex claude; do
  container="light-agent-$adapter-personal-workflow"
  docker exec -i "$container" sh -s -- "$adapter" <<'SH'
set -eu
adapter=$1
endpoint=https://light-workflow:8449/internal/workflow/jobs/poll
host=01964b05-552a-7c4b-9184-6857e7f3dc5f
cert="/run/workflow-actions/$adapter-client-identity.pem"
ca=/run/workflow-actions/ca.pem
result=$({ printf 'x-scope-token: '; cat /run/workflow-actions/scope-token; printf '\n'; } |
  curl --silent --show-error --max-time 10 --cacert "$ca" --cert "$cert" --key "$cert" \
    --header @- --header 'content-type: application/json' --data "{\"hostId\":\"$host\"}" \
    --write-out '|%{http_code}' "$endpoint")
test "$result" = '[]|200'
status=$({ printf 'x-scope-token: '; cat /run/workflow-actions/scope-token; printf '\n'; } |
  curl --silent --show-error --max-time 10 --cacert "$ca" --cert "$cert" --key "$cert" \
    --header @- --header 'content-type: application/json' \
    --data '{"hostId":"11111111-1111-4111-8111-111111111111"}' \
    --output /dev/null --write-out '%{http_code}' "$endpoint")
test "$status" = 403
status=$(curl --silent --show-error --max-time 10 --cacert "$ca" --cert "$cert" --key "$cert" \
  --header 'content-type: application/json' --data "{\"hostId\":\"$host\"}" \
  --output /dev/null --write-out '%{http_code}' "$endpoint")
test "$status" = 403
printf '%s: authenticated empty poll 200; wrong Host and missing scope 403\n' "$adapter"
SH
done
echo 'Transport identity gates passed; no native job or grant was created.'
