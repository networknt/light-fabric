#!/usr/bin/env bash
set -euo pipefail

: "${LLM_HOST_ID:?LLM_HOST_ID is required}"
: "${LLM_ENVIRONMENT:?LLM_ENVIRONMENT is required}"
: "${LLM_GATEWAY_NAMESPACE:?LLM_GATEWAY_NAMESPACE is required}"
: "${LLM_GATEWAY_WORKLOAD:?LLM_GATEWAY_WORKLOAD is required}"
: "${LLM_INVENTORY_GENERATION:?LLM_INVENTORY_GENERATION is required}"
: "${LLM_PORTAL_COMMAND_URL:?LLM_PORTAL_COMMAND_URL is required}"
: "${LLM_DEPLOYMENT_AUTOMATION_TOKEN_FILE:?LLM_DEPLOYMENT_AUTOMATION_TOKEN_FILE is required}"

work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
workload_json="$work/workload.json"
pods_json="$work/pods.json"
stable_workload_json="$work/workload-stable.json"
stable_pods_json="$work/pods-stable.json"
request_json="$work/request.json"

kubectl -n "$LLM_GATEWAY_NAMESPACE" get deployment "$LLM_GATEWAY_WORKLOAD" -o json > "$workload_json"
selector="$(jq -r '.spec.selector.matchLabels | to_entries | map("\(.key)=\(.value)") | join(",")' "$workload_json")"
kubectl -n "$LLM_GATEWAY_NAMESPACE" get pods -l "$selector" -o json > "$pods_json"

desired="$(jq -r .spec.replicas "$workload_json")"
observed="$(jq -r '.status.observedGeneration // 0' "$workload_json")"
generation="$(jq -r .metadata.generation "$workload_json")"
updated="$(jq -r '.status.updatedReplicas // 0' "$workload_json")"
available="$(jq -r '.status.availableReplicas // 0' "$workload_json")"
ready="$(jq -r '.status.readyReplicas // 0' "$workload_json")"
replicas="$(jq -r '.status.replicas // 0' "$workload_json")"
unavailable="$(jq -r '.status.unavailableReplicas // 0' "$workload_json")"
pod_count="$(jq '.items | length' "$pods_json")"
ready_pods="$(jq '[.items[] | select(.metadata.deletionTimestamp == null) | select(.status.phase=="Running") | select(.status.conditions[]? | select(.type=="Ready" and .status=="True"))] | length' "$pods_json")"
revision_count="$(jq '[.items[].metadata.labels["pod-template-hash"]] | unique | length' "$pods_json")"
test "$desired" -gt 0
test "$observed" -eq "$generation"
test "$updated" -eq "$desired" -a "$available" -eq "$desired" -a "$ready" -eq "$desired"
test "$replicas" -eq "$desired" -a "$unavailable" -eq 0
test "$pod_count" -eq "$desired" -a "$ready_pods" -eq "$desired" -a "$revision_count" -eq 1

# A single Ready observation can catch the transient midpoint of a rollout.
# Require the exact workload generation/resourceVersion and pod UID set to
# remain stable for a bounded observation window before freezing inventory.
sleep "${LLM_ROLLOUT_STABILITY_SECONDS:-10}"
kubectl -n "$LLM_GATEWAY_NAMESPACE" get deployment "$LLM_GATEWAY_WORKLOAD" -o json > "$stable_workload_json"
kubectl -n "$LLM_GATEWAY_NAMESPACE" get pods -l "$selector" -o json > "$stable_pods_json"
test "$(jq -r .metadata.generation "$stable_workload_json")" = "$generation"
test "$(jq -r .metadata.resourceVersion "$stable_workload_json")" = "$(jq -r .metadata.resourceVersion "$workload_json")"
test "$(jq -c '[.items[].metadata.uid] | sort' "$stable_pods_json")" = "$(jq -c '[.items[].metadata.uid] | sort' "$pods_json")"

inventory_id="${LLM_REPLICA_INVENTORY_ID:-$(uuidgen)}"
valid_from="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
valid_until="${LLM_INVENTORY_VALID_UNTIL:?LLM_INVENTORY_VALID_UNTIL is required}"
jq -n \
  --arg hostId "$LLM_HOST_ID" --arg environment "$LLM_ENVIRONMENT" \
  --arg replicaInventoryId "$inventory_id" --argjson inventoryGeneration "$LLM_INVENTORY_GENERATION" \
  --argjson desiredCount "$desired" --arg workloadName "$LLM_GATEWAY_WORKLOAD" \
  --arg workloadUid "$(jq -r .metadata.uid "$workload_json")" \
  --arg workloadResourceVersion "$(jq -r .metadata.resourceVersion "$workload_json")" \
  --arg validFrom "$valid_from" --arg validUntil "$valid_until" \
  --slurpfile pods "$pods_json" \
  '{hostId:$hostId,environment:$environment,replicaInventoryId:$replicaInventoryId,
    inventoryGeneration:$inventoryGeneration,inventoryDigest:"",
    desiredCount:$desiredCount,workloadKind:"Deployment",workloadName:$workloadName,
    workloadUid:$workloadUid,workloadResourceVersion:$workloadResourceVersion,
    validFrom:$validFrom,validUntil:$validUntil,
    members:($pods[0].items | map(select(.status.conditions[]? | select(.type=="Ready" and .status=="True")) |
      {gatewayInstance:.metadata.uid,podUid:.metadata.uid,namespace:.metadata.namespace,
       serviceAccount:.spec.serviceAccountName,required:true}) | sort_by(.gatewayInstance))}' > "$request_json"

duplicates="$(jq '[.members[].gatewayInstance] | length - (unique | length)' "$request_json")"
test "$duplicates" -eq 0
canonical_inventory="$(jq -cS '.inventoryDigest=""' "$request_json")"
digest="$(printf '%s' "$canonical_inventory" | sha256sum | cut -d' ' -f1)"
jq --arg digest "$digest" '.inventoryDigest=$digest' "$request_json" > "$work/final.json"
jq '{host:"lightapi.net",service:"genai",action:"recordLlmGatewayReplicaInventory",version:"0.1.0",data:.}' \
  "$work/final.json" > "$work/rpc.json"

curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $(tr -d '\r\n' < "$LLM_DEPLOYMENT_AUTOMATION_TOKEN_FILE")" \
  -H 'Content-Type: application/json' \
  --data-binary @"$work/rpc.json" "$LLM_PORTAL_COMMAND_URL"
echo
