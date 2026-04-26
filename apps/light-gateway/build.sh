#!/bin/bash
set -euo pipefail

if [ "${DEBUG:-false}" = "true" ]; then
  set -x
fi

IMAGE_NAME="networknt/light-gateway"
VERSION=""
LOCAL_BUILD=false
NO_CACHE_ARG=""
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

show_help() {
  echo " "
  echo "Error: $1"
  echo " "
  echo "    build.sh [VERSION] [-l|--local] [--no-cache]"
  echo " "
  echo "    where [VERSION] is the Docker image version to build and publish"
  echo "          [-l|--local] optionally builds the image locally without pushing"
  echo "          [--no-cache] optionally builds the image without using the Docker build cache"
  echo " "
  echo "    example: ./build.sh 0.1.0"
  echo "    example: ./build.sh 0.1.0 -l"
  echo "    example: ./build.sh -l 0.1.0"
  echo "    example: ./build.sh 0.1.0 --no-cache"
  echo " "
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -l|--local)
      LOCAL_BUILD=true
      shift
      ;;
    --no-cache)
      NO_CACHE_ARG="--no-cache"
      shift
      ;;
    -*)
      show_help "Invalid option: $1"
      exit 1
      ;;
    *)
      if [[ -z "$VERSION" ]]; then
        VERSION="$1"
      else
        show_help "Invalid option: $1"
        exit 1
      fi
      shift
      ;;
  esac
done

if [[ -z "$VERSION" ]]; then
  show_help "[VERSION] parameter is missing"
  exit 1
fi

echo "Building Docker image with version ${VERSION}"
docker build ${NO_CACHE_ARG} -t "${IMAGE_NAME}:${VERSION}" -t "${IMAGE_NAME}:latest" -f "${SCRIPT_DIR}/docker/Dockerfile" "${REPO_ROOT}"
echo "Images built with version ${VERSION}"

if $LOCAL_BUILD; then
  echo "Skipping DockerHub publish due to local build flag (-l or --local)"
else
  echo "Pushing Docker image tags ${IMAGE_NAME}:${VERSION} and ${IMAGE_NAME}:latest"
  docker push "${IMAGE_NAME}:${VERSION}"
  docker push "${IMAGE_NAME}:latest"
  echo "Images pushed successfully"
fi
