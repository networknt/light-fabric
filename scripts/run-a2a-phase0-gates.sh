#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
portal_dir=${LIGHT_PORTAL_DIR:-"$(dirname "$repo_dir")/light-portal"}
rust_fixture="$repo_dir/contracts/a2a/phase0/canonical-projection.json"
java_fixture="$portal_dir/db-provider/src/test/resources/a2a-phase0/canonical-projection.json"

if [[ ! -d "$portal_dir/db-provider" ]]; then
  echo "light-portal db-provider not found at $portal_dir" >&2
  exit 1
fi

cmp --silent "$rust_fixture" "$java_fixture" || {
  echo "Java and Rust A2A Phase 0 fixtures differ" >&2
  exit 1
}

(
  cd "$repo_dir"
  cargo fmt --all -- --check
  cargo test -p a2a-core
  cargo test -p light-a2a --lib
  cargo test -p light-agent agent_config --lib
  cargo test -p light-agent embedded_runtime_templates_resolve_and_match_typed_configs
  cargo check -p light-agent -p light-a2a
)

(
  cd "$portal_dir"
  mvn -pl db-provider -am \
    -Dtest=A2aPublicationSupportTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
)

echo "A2A Phase 0 contract gates passed"
