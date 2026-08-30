#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

bash "$script_dir/validate-operational-store.sh"
bash "$script_dir/retire-configserver-execution-authority.sh"

echo "Phase 3 execution destination is ready and Config Server write authority is retired."
