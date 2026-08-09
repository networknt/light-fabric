#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir="${LMT_REPORT_DIR:-$repo_root/target/lmt-reports}"
mode="${1:---implementation}"
case "$mode" in --implementation|--production) ;; *) echo "usage: $0 [--implementation|--production]" >&2; exit 2;; esac
mkdir -p "$report_dir"

LMT_REPORT_DIR="$report_dir" "$repo_root/scripts/run-llm-local-transport-contract-gates.sh"
if [[ -n "${LMT_DATABASE_URL:-}" ]]; then
  LMT_REPORT_DIR="$report_dir" "$repo_root/scripts/run-llm-local-transport-admission-gates.sh"
elif [[ "$mode" == "--production" ]]; then
  echo "LMT_DATABASE_URL is required for a production release gate" >&2
  exit 1
fi
LMT_REPORT_DIR="$report_dir" "$repo_root/scripts/run-llm-local-transport-client-gates.sh"
LMT_REPORT_DIR="$report_dir" "$repo_root/scripts/run-model-provider-sidecar-gates.sh"
LMT_REPORT_DIR="$report_dir" "$repo_root/scripts/run-llm-live-qualification-gates.sh"
(cd "$repo_root" && ./scripts/run-llm-production-integration-gates.sh)
(cd "$repo_root" && ./scripts/run-llm-release-qualification-gates.sh)
(cd "$repo_root" && ./scripts/run-llm-rollout-gates.sh)

if [[ "$mode" == "--production" ]]; then
  evidence_dir="${LMT_PRODUCTION_EVIDENCE_DIR:-}"
  test -n "$evidence_dir" -a -d "$evidence_dir"
  required=(replica-convergence runtime-matrix rotation-exercises network-isolation client-pool-headroom paid-fallback zero-price-ledger lane-isolation embedding-space sensitive-data-scan paired-rollback)
  for report in "${required[@]}"; do
    test "$(jq -r .status "$evidence_dir/$report.json")" = "PASS"
  done
  test "$(find "${LMT_ACK_OUTBOX_DIR:?LMT_ACK_OUTBOX_DIR is required}" -maxdepth 1 -type f | wc -l)" -eq 0
  promotion="PASS"
else
  promotion="IMPLEMENTATION_ONLY"
fi

jq -n --arg gate "LMT-G5" --arg status "$promotion" --arg revision "$(git -C "$repo_root" rev-parse HEAD)" \
  --arg mode "$mode" '{schemaVersion:"1",gate:$gate,status:$status,mode:$mode,sourceRevision:$revision,productionPromotionAllowed:($status=="PASS")}' \
  > "$report_dir/lmt-g5-release.json"
echo "LMT-G5 $promotion: $report_dir/lmt-g5-release.json"
