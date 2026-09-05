#!/usr/bin/env bash
set -euo pipefail

readonly EXPECTED_VERSION="0.153.2"
readonly EXPECTED_SCHEMA_SHA256="d3eace08be5dca386bfd1f1e8df650058b4113f1e10870a284d775d75517576a"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <empty-output-directory>" >&2
  exit 64
fi

readonly output_directory="$1"
if [[ -e "$output_directory" ]]; then
  echo "output directory already exists: $output_directory" >&2
  exit 65
fi
if [[ "$(codex --version)" != "codex-cli ${EXPECTED_VERSION}" ]]; then
  echo "codex version differs from the pinned schema generator" >&2
  exit 66
fi

mkdir -p "$output_directory"
codex app-server generate-json-schema --out "$output_directory/json"
codex app-server generate-ts --out "$output_directory/typescript"
echo "${EXPECTED_SCHEMA_SHA256}  $output_directory/json/codex_app_server_protocol.v2.schemas.json" \
  | sha256sum --check --status
