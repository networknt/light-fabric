#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 ]]; then
  echo "usage: bash apps/light-lambda/scripts/compare-local.sh <fresh-output-dir> <java-result-dir>" >&2
  exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
output=$(realpath -m -- "$1")
java_results=$(realpath -- "$2")
mkdir -- "$output"
cd -- "$repo"
selection=(
  --fixture E-03-response-off-0-multi-v1
  --fixture E-03-response-off-1-multi-v1
  --fixture E-03-response-off-2-multi-v1
  --fixture E-03-response-off-3-multi-v1
  --fixture Q-02-empty-correlation-multi-contract-v1
  --fixture Q-02-repeated-correlation-multi-contract-v1
  --fixture B-04-Q-02-header-prefix-correlation-multi-contract-v1
  --fixture B-04-remove-update-prefix-collision-multi-contract-v1
)
rtk cargo build --locked -p light-lambda --bin bootstrap
rtk cargo run --locked -p serverless-conformance -- run "${selection[@]}" --out "$output/rust"
rtk cargo run --locked -p serverless-conformance -- compare "${selection[@]}" --java "$java_results" --rust "$output/rust" --out "$output/comparison"
