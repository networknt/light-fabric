#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
manifest="$repo_root/benchmarks/llm-gateway/manifests/sdk-embeddings-smoke-manifest.json"
output="${LLM_SDK_EMBEDDING_EVIDENCE_OUTPUT:-$repo_root/benchmarks/llm-gateway/reports/sdk-embeddings-smoke.json}"

for name in LLM_SDK_BASE_URL LLM_SDK_API_KEY LLM_SDK_EMBEDDING_MODEL LLM_SDK_EMBEDDING_DIMENSIONS LLM_SDK_REVISION LLM_SDK_PROJECTION_DIGEST LLM_SDK_EMBEDDING_CONFORMANCE_DIGEST; do
  if [[ -z "${!name:-}" ]]; then
    echo "$name is required" >&2
    exit 2
  fi
done
if [[ ! "$LLM_SDK_REVISION" =~ ^[0-9a-f]{40}$ ]]; then
  echo "LLM_SDK_REVISION must be the 40-character release commit" >&2
  exit 2
fi
for name in LLM_SDK_PROJECTION_DIGEST LLM_SDK_EMBEDDING_CONFORMANCE_DIGEST; do
  if [[ ! "${!name}" =~ ^[0-9a-f]{64}$ ]]; then
    echo "$name must be a 64-character lowercase SHA-256 digest" >&2
    exit 2
  fi
done
if [[ ! "$LLM_SDK_EMBEDDING_DIMENSIONS" =~ ^[1-9][0-9]*$ ]]; then
  echo "LLM_SDK_EMBEDDING_DIMENSIONS must be a positive integer" >&2
  exit 2
fi

scratch="$(mktemp -d)"
cleanup() { find "$scratch" -depth -delete; }
trap cleanup EXIT

python_version="$(jq -r '.clients.python.version' "$manifest")"
typescript_version="$(jq -r '.clients.typescript.version' "$manifest")"
python3 -m venv "$scratch/venv"
"$scratch/venv/bin/pip" install --disable-pip-version-check --quiet "openai==$python_version"
"$scratch/venv/bin/python" "$repo_root/benchmarks/llm-gateway/sdk-smoke/python_embeddings_smoke.py" >"$scratch/python.json"

mkdir -p "$scratch/typescript"
cp "$repo_root/benchmarks/llm-gateway/sdk-smoke/typescript_embeddings_smoke.mjs" "$scratch/typescript/smoke.mjs"
npm install --ignore-scripts --no-audit --no-fund --prefix "$scratch/typescript" "openai@$typescript_version" >/dev/null
node "$scratch/typescript/smoke.mjs" >"$scratch/typescript.json"

mkdir -p "$(dirname -- "$output")"
jq -n \
  --arg revision "$LLM_SDK_REVISION" \
  --arg generatedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg projectionDigest "$LLM_SDK_PROJECTION_DIGEST" \
  --arg embeddingConformanceDigest "$LLM_SDK_EMBEDDING_CONFORMANCE_DIGEST" \
  --slurpfile python "$scratch/python.json" \
  --slurpfile typescript "$scratch/typescript.json" \
  '{
    schemaVersion: "1",
    revision: $revision,
    generatedAt: $generatedAt,
    projectionDigest: $projectionDigest,
    conformanceDigest: $embeddingConformanceDigest,
    sanitized: true,
    secretMaterialRecorded: false,
    clients: [$python[0], $typescript[0]],
    status: (if ($python[0].status == "pass" and $typescript[0].status == "pass") then "pass" else "fail" end)
  }' >"$output"

echo "wrote $output"
