#!/bin/sh
# Builds the worker image for sandbox level `container` (see README.md).
#
# usage: swarm/sandbox/build-image.sh [IMAGE_NAME]
#
# Settings by environment:
#   SOMMELIER_SANDBOX_RUNTIME   podman or docker (default: podman if found, else docker)
#   SOMMELIER_BASE_IMAGE        base image, pin it with an @sha256 digest for a build you can repeat
#   SOMMELIER_TOOL_PACKAGES     npm packages of the vendor tools to put in the image, for example
#                               "@anthropic-ai/claude-code". Empty means the image holds no vendor tool.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
image="${1:-sommelier-worker:latest}"

runtime="${SOMMELIER_SANDBOX_RUNTIME:-}"
if [ -z "$runtime" ]; then
  if command -v podman >/dev/null 2>&1; then
    runtime=podman
  elif command -v docker >/dev/null 2>&1; then
    runtime=docker
  else
    echo "build-image.sh: no container runtime found: install rootless podman or docker" >&2
    exit 1
  fi
fi

toolchain="$root/bus/rust-toolchain.toml"
if [ ! -f "$toolchain" ]; then
  echo "build-image.sh: $toolchain not found: run this script from a full checkout" >&2
  exit 1
fi
rust_version=$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' "$toolchain" | head -n 1)
if [ -z "$rust_version" ]; then
  echo "build-image.sh: no channel in $toolchain" >&2
  exit 1
fi

base="${SOMMELIER_BASE_IMAGE:-docker.io/library/rust:${rust_version}-slim-bookworm}"

"$runtime" build \
  --file "$here/Containerfile" \
  --build-arg "BASE_IMAGE=$base" \
  --build-arg "TOOL_PACKAGES=${SOMMELIER_TOOL_PACKAGES:-}" \
  --tag "$image" \
  "$here"
echo "built $image from $base"
