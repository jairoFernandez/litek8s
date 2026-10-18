#!/usr/bin/env bash
# Cross-build a linux release binary of litek8s inside the same rust image that
# `litek8s node add --install` uses, so the benchmark measures what a real node runs.
# Usage: bench/build-linux.sh [arm64|amd64]   -> prints the artifact path
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
docker run --rm --platform "linux/${ARCH}" \
  -u "$(id -u):$(id -g)" \
  -e "CARGO_TARGET_DIR=/work/${TARGET_DIR}" \
  -v "${ROOT}:/work" -w /work \
  rust:1.90 cargo build --release >&2
echo "${ROOT}/${TARGET_DIR}/release/litek8s"
