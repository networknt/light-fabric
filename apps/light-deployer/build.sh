#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-latest}"
IMAGE="${IMAGE:-networknt/light-deployer:${VERSION}}"

cd "$(dirname "$0")/../.."
docker build -f apps/light-deployer/Dockerfile -t "${IMAGE}" .
