#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

bash "$script_dir/validate-phase4-agent-authority.sh"
bash "$script_dir/retire-configserver-workflow-authority.sh"

echo "Phase 5 Workflow/A2A, Phase 4 Agent, and Phase 3 execution destinations are ready; Config Server operational write authority is retired."
