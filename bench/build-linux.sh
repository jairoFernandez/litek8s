#!/usr/bin/env bash
# Cross-build a linux release binary of litek8s inside the rust:1.90 image, the
# toolchain the release build pins (releases are static musl builds in
# rust:1.90-alpine, see .github/workflows/release.yml; this one links glibc).
# Usage: bench/build-linux.sh [arm64|amd64]   -> prints the artifact path
#
# Any Docker API does, litek8s's own included (DOCKER_HOST=unix://$HOME/.litek8s/docker.sock,
# docs/develop-without-docker.md; amd64 then runs under Rosetta in the dev VM).
#   LITEK8S_BUILD_VOLUME=<name>  keep cargo's target dir and registry in that
#       Docker volume instead of target/ in the bind-mounted checkout; only the
#       binary is copied out (to the same path).
#   CARGO_BUILD_JOBS=<n>         passed to cargo in the container.
#
# With BENCH_BUILD_VM=<vm name> (a bench/vm.sh VM, e.g. `bench/vm.sh up lkbuild
# 2420 4096 6` with rustup installed) the source is synced into that VM and
# built natively there instead (arm64 only), without Docker.
set -euo pipefail
ARCH="${1:-arm64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="target/bench-linux-${ARCH}"
if [[ -n "${BENCH_BUILD_VM:-}" ]]; then
  [[ "$ARCH" == arm64 ]] || { echo "BENCH_BUILD_VM builds arm64 only" >&2; exit 2; }
  VM="$ROOT/bench/vm.sh"
  port="$(cat "${BENCH_WORK:-$ROOT/bench/.work}/vms/$BENCH_BUILD_VM/port")"
  key="$("$VM" key)"
  rsync -a --delete --exclude target --exclude .git --exclude 'bench/.work' --exclude 'compat/.work' --exclude 'bench/results' --exclude 'compat/results' \
    -e "ssh -i $key -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p $port" \
    "$ROOT/" bench@127.0.0.1:litek8s/ >&2
  "$VM" ssh "$BENCH_BUILD_VM" 'cd litek8s && ~/.cargo/bin/cargo build --release 2>&1 | tail -3' >&2
  mkdir -p "$ROOT/$TARGET_DIR/release"
  "$VM" ssh "$BENCH_BUILD_VM" 'cat litek8s/target/release/litek8s' >"$ROOT/$TARGET_DIR/release/litek8s.tmp"
  chmod +x "$ROOT/$TARGET_DIR/release/litek8s.tmp"
  mv "$ROOT/$TARGET_DIR/release/litek8s.tmp" "$ROOT/$TARGET_DIR/release/litek8s"
  echo "$ROOT/$TARGET_DIR/release/litek8s"
  exit 0
fi
# Runs as root and hands the output to you afterwards: on the macOS dev VM's
# virtiofs share, a non-root uid sees directories as root's for a moment while
# they change and cargo fails with EACCES (root is not checked).
args=(--rm --platform "linux/${ARCH}" -v "${ROOT}:/work" -w /work)
[[ -n "${CARGO_BUILD_JOBS:-}" ]] && args+=(-e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}")
OWNER="$(id -u):$(id -g)"
if [[ -n "${LITEK8S_BUILD_VOLUME:-}" ]]; then
  mkdir -p "$ROOT/$TARGET_DIR/release"
  docker run "${args[@]}" -v "${LITEK8S_BUILD_VOLUME}:/cache" \
    -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo-home \
    rust:1.90 sh -c "cargo build --release && cp /cache/target/release/litek8s /work/${TARGET_DIR}/release/litek8s.tmp && chown ${OWNER} /work/${TARGET_DIR}/release/litek8s.tmp" >&2
  mv "$ROOT/$TARGET_DIR/release/litek8s.tmp" "$ROOT/$TARGET_DIR/release/litek8s"
else
  docker run "${args[@]}" -e "CARGO_TARGET_DIR=/work/${TARGET_DIR}" \
    rust:1.90 sh -c "cargo build --release; rc=\$?; chown -R ${OWNER} /work/${TARGET_DIR}; exit \$rc" >&2
fi
echo "${ROOT}/${TARGET_DIR}/release/litek8s"
