#!/usr/bin/env bash
# Cross-build a linux release binary of litek8s inside the same rust image that
# `litek8s node add --install` uses, so the benchmark measures what a real node runs.
# Usage: bench/build-linux.sh [arm64|amd64]   -> prints the artifact path
set -euo pipefail
ARCH="${1:-arm64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="target/bench-linux-${ARCH}"
docker run --rm --platform "linux/${ARCH}" \
  -u "$(id -u):$(id -g)" \
  -e "CARGO_TARGET_DIR=/work/${TARGET_DIR}" \
  -v "${ROOT}:/work" -w /work \
  rust:1.90 cargo build --release >&2
echo "${ROOT}/${TARGET_DIR}/release/litek8s"
