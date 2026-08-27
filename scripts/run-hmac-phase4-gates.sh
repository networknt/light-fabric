#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
redis_container="light-fabric-hmac-phase4-$$"

cleanup() {
  docker rm -f "$redis_container" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cd "$repo_dir"
./scripts/run-hmac-phase3-gates.sh

# Keep the cross-runtime contract visible even though the cumulative Phase 3
# suite also discovers these tests.
cargo test --quiet --locked -p light-pingora shared_java_rust
cargo test --quiet --locked -p light-gateway \
  phase4_github_to_counting_jenkins_preserves_body_and_replay_lifecycle
cargo test --quiet --locked -p light-gateway \
  phase4_composed_api_key_and_hmac_reach_upstream_only_when_both_verify
cargo test --quiet --locked -p light-gateway \
  hmac_reload_swaps_only_a_fully_compiled_candidate

docker run --detach --rm --name "$redis_container" \
  --publish 127.0.0.1::6379 redis:7-alpine >/dev/null
for _ in $(seq 1 50); do
  if docker exec "$redis_container" redis-cli ping 2>/dev/null | grep -q '^PONG$'; then
    break
  fi
  sleep 0.1
done
docker exec "$redis_container" redis-cli ping | grep -q '^PONG$'
redis_address="$(docker port "$redis_container" 6379/tcp | tail -n 1)"
HMAC_PHASE4_REDIS_URL="redis://$redis_address/" \
  cargo test --quiet --locked -p light-pingora \
  redis_store_is_atomic_across_independent_provider_connections -- --ignored

cargo fmt --all --check
git diff --check

echo "HMAC Phase 4 local protocol, Redis-provider, lifecycle, conformance, counting-upstream, and rotation gates passed."
