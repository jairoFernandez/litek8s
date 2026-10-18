#!/usr/bin/env bash
# Build the static (musl) linux/arm64 litek8s that runs as PID 1 of the macOS
# dev VM, in a rust:alpine container. Prints the binary's path.
# Usage: dev/vm/build-guest-binary.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET_DIR="target/guest-musl-arm64"
docker run --rm --platform linux/arm64 \
  -e "CARGO_TARGET_DIR=/work/${TARGET_DIR}" -e CARGO_HOME=/work/target/guest-cargo-home -e CARGO_PROFILE_RELEASE_STRIP=symbols \
  -v "${ROOT}:/work" -w /work \
  rust:1.90-alpine sh -c 'apk add -q musl-dev gcc g++ make perl >/dev/null && cargo build --release --no-default-features && chown -R '"$(id -u):$(id -g)"' /work/target/guest-musl-arm64 /work/target/guest-cargo-home' >&2
echo "${ROOT}/${TARGET_DIR}/release/litek8s"
