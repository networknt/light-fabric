#!/usr/bin/env bash
# Read-only local restart barrier. The owner must also exclude external writers.
set -euo pipefail
operations_root="${1:?operations root required}"
state_dir="$operations_root/.runtime/w7"
fail() { echo 'W7_STARTUP_REFUSED:PREPARATION_MISSING_BUSY_OR_CHANGED' >&2; exit 2; }
[[ -d "$state_dir" && -s "$state_dir/startup-ready.sha256" ]] || fail
if [[ "${2:-}" != '--lock-held' ]]; then
  exec 8>"$state_dir/w7.lock"
  flock -n 8 || fail
fi
[[ -s "$state_dir/prepared.json" && -f "$operations_root/w7-assets.json" ]] || fail
python3 -B "$operations_root/bin/w7_rollout.py" verify-assets --assets "$operations_root" >/dev/null || fail
(
  cd -- "$operations_root"
  sha256sum -c "$state_dir/startup-ready.sha256" >/dev/null 2>&1 || exit 2
  cd bundle
  sha256sum -c bundle.sha256 >/dev/null 2>&1 || exit 2
) || fail
echo 'W7_STARTUP_PREPARATION_VERIFIED'
