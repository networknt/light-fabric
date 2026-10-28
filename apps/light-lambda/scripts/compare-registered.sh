#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 && $# != 3 ]]; then
  echo "usage: bash apps/light-lambda/scripts/compare-registered.sh <fresh-output-dir> <java-result-dir> [registered-v3|registered-v2|router-forwarding-v1]" >&2
  exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
output=$(realpath -m -- "$1")
java_results=$(realpath -- "$2")
profile=${3:-registered-v3}
mkdir -- "$output"
cd -- "$repo"
rtk cargo build --locked -p light-lambda --bin bootstrap
run_status=0
compare_status=0
rtk cargo run --locked -p serverless-conformance -- run --profile "$profile" --out "$output/rust" || run_status=$?
rtk cargo run --locked -p serverless-conformance -- compare --profile "$profile" --java "$java_results" --rust "$output/rust" --out "$output/comparison" || compare_status=$?
echo "runExit=$run_status compareExit=$compare_status (native failures remain failures)"
if (( run_status != 0 || compare_status != 0 )); then exit 1; fi
