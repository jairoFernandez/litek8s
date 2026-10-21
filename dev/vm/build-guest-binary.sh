#!/usr/bin/env bash
# Build the static (musl) linux/arm64 litek8s that runs as PID 1 of the macOS
# dev VM, in a rust:alpine container. Prints the binary's path.
# Usage: dev/vm/build-guest-binary.sh
#
# Any Docker API does: OrbStack, Docker Desktop, or litek8s's own
# (DOCKER_HOST=unix://$HOME/.litek8s/docker.sock, docs/develop-without-docker.md).
#   LITEK8S_BUILD_VOLUME=<name>  keep cargo's target dir and registry in that
#       Docker volume instead of target/ in the bind-mounted checkout; only the
#       binary is copied out (to the same path).
#   CARGO_BUILD_JOBS=<n>         passed to cargo in the container.
#   LITEK8S_RELEASE=1            a release build (`--version` without DEV <commit>).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET_DIR="target/guest-musl-arm64"
SETUP='apk add -q musl-dev gcc g++ make perl >/dev/null'
args=(--rm --platform linux/arm64 -e CARGO_PROFILE_RELEASE_STRIP=symbols -v "${ROOT}:/work" -w /work)
[[ -n "${CARGO_BUILD_JOBS:-}" ]] && args+=(-e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}")
[[ -n "${LITEK8S_RELEASE:-}" ]] && args+=(-e "LITEK8S_RELEASE=${LITEK8S_RELEASE}")
# The container has the sources but not the repository (build.rs: DEV <commit>).
COMMIT="${LITEK8S_GIT_COMMIT:-$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)}"
[[ -n "$COMMIT" ]] && args+=(-e "LITEK8S_GIT_COMMIT=${COMMIT}")
if [[ -n "${LITEK8S_BUILD_VOLUME:-}" ]]; then
  mkdir -p "${ROOT}/${TARGET_DIR}/release"
  docker run "${args[@]}" -v "${LITEK8S_BUILD_VOLUME}:/cache" \
    -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo-home \
    rust:1.90-alpine sh -c "${SETUP} && cargo build --release && cp /cache/target/release/litek8s /work/${TARGET_DIR}/release/litek8s.tmp && chown $(id -u):$(id -g) /work/${TARGET_DIR}/release/litek8s.tmp" >&2
  mv "${ROOT}/${TARGET_DIR}/release/litek8s.tmp" "${ROOT}/${TARGET_DIR}/release/litek8s"
else
  docker run "${args[@]}" \
    -e "CARGO_TARGET_DIR=/work/${TARGET_DIR}" -e CARGO_HOME=/work/target/guest-cargo-home \
    rust:1.90-alpine sh -c "${SETUP} && cargo build --release && chown -R $(id -u):$(id -g) /work/${TARGET_DIR} /work/target/guest-cargo-home" >&2
fi
echo "${ROOT}/${TARGET_DIR}/release/litek8s"
