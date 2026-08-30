#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

bash "$script_dir/validate-phase3-execution-authority.sh"
bash "$script_dir/retire-configserver-agent-authority.sh"

echo "Phase 4 Agent and Phase 3 execution destinations are ready; Config Server write authority is retired."
