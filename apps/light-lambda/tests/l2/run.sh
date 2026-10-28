#!/usr/bin/env bash
set -euo pipefail
script_dir="$(rtk proxy dirname -- "${BASH_SOURCE[0]}")"
exec rtk proxy python3 -B "$script_dir/run.py" "$@"
