#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
portal_dir=${LIGHT_PORTAL_DIR:-"$(dirname "$repo_dir")/light-portal"}

if [[ ! -d "$portal_dir/db-provider" ]]; then
  echo "light-portal db-provider not found at $portal_dir" >&2
  exit 1
fi

(
  cd "$repo_dir"
  cargo fmt --all -- --check
  cargo test -p a2a-client
  cargo test -p a2a-protocol
  cargo test -p a2a-core
  cargo test -p light-pingora a2a::tests --lib
  cargo test -p light-a2a --lib
  cargo test -p light-agent --lib
  cargo test -p light-agent --bin light-agent native_a2a_application_errors_use_http_200
  cargo test -p light-agent embedded_runtime_templates_resolve_and_match_typed_configs
  cargo check -p light-gateway -p light-agent -p light-a2a
)

(
  cd "$portal_dir"
  mvn -pl db-provider -am \
    -Dtest=A2aPublicationSupportTest \
    -Dsurefire.failIfNoSpecifiedTests=false test
)

echo "A2A Phase 1 foundation gates passed"
