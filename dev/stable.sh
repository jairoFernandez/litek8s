#!/usr/bin/env bash
# A pinned litek8s for developing litek8s itself: `litek8s dev` (the cluster
# and the Docker API that cargo test, the corpus and the Linux builds use)
# runs from a known-good revision, never from the checkout being edited.
# See docs/develop-without-docker.md.
#
#   dev/stable.sh install [rev]   build <rev> (default HEAD) into $LITEK8S_STABLE
#   dev/stable.sh up [dev up args] | down | status
#   dev/stable.sh env             eval "$(dev/stable.sh env)": DOCKER_HOST, KUBECONFIG
#
# LITEK8S_STABLE (default ~/.litek8s/stable) holds bin/litek8s, vm/{Image,initramfs.gz},
# REVISION and the cluster's kubeconfig (~/.kube/config is left alone).
# `install` builds the macOS binary with this machine's cargo (target dir:
# target/stable-host of this checkout) and the VM's Linux binary in a
# rust:alpine container on whatever Docker API is current: the running stable
# cluster's when there is one, else DOCKER_HOST / the docker context (the very
# first install, e.g. OrbStack). Its cargo cache stays in the Docker volume
# litek8s-guest-build.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STABLE="${LITEK8S_STABLE:-$HOME/.litek8s/stable}"
SOCK="$HOME/.litek8s/docker.sock"
export KUBECONFIG="$STABLE/kubeconfig"

dev() { LITEK8S_DEV_VM_ASSETS="$STABLE/vm" "$STABLE/bin/litek8s" dev "$@"; }

install() {
  local rev src new
  rev="$(git -C "$ROOT" rev-parse --verify "${1:-HEAD}^{commit}")"
  # The guest build bind-mounts the sources: they must be under $HOME, which
  # the dev VM shares.
  src="$STABLE.src"
  new="$STABLE.new"
  rm -rf "$src" "$new"
  mkdir -p "$src" "$new/bin"
  git -C "$ROOT" archive "$rev" | tar -x -C "$src"
  echo "building litek8s ${rev:0:12} for macOS" >&2
  (cd "$src" && CARGO_TARGET_DIR="$ROOT/target/stable-host" cargo build --release --locked)
  cp "$ROOT/target/stable-host/release/litek8s" "$new/bin/litek8s"
  if [[ -S "$SOCK" ]] && DOCKER_HOST="unix://$SOCK" docker version >/dev/null 2>&1; then
    export DOCKER_HOST="unix://$SOCK"
  fi
  echo "building the dev VM's guest on ${DOCKER_HOST:-the current docker context}" >&2
  LITEK8S_BUILD_VOLUME=litek8s-guest-build "$src/dev/vm/build-guest-binary.sh" >/dev/null
  python3 "$src/dev/vm/build-guest.py" --litek8s "$src/target/guest-musl-arm64/release/litek8s" --out "$new/vm" --cache "$ROOT/target/guest-apk-cache" >&2
  rm -rf "$new/vm/build"
  echo "$rev" >"$new/REVISION"
  [[ -f "$STABLE/kubeconfig" ]] && cp "$STABLE/kubeconfig" "$new/kubeconfig"
  rm -rf "$STABLE.old"
  [[ -d "$STABLE" ]] && mv "$STABLE" "$STABLE.old"
  mv "$new" "$STABLE"
  rm -rf "$STABLE.old" "$src"
  echo "installed $rev in $STABLE (a running cluster keeps the old one until: dev/stable.sh down && dev/stable.sh up)" >&2
}

case "${1:-}" in
  install) shift; install "$@" ;;
  up) shift; dev up "$@" ;;
  down | status) dev "$1" ;;
  env)
    echo "export DOCKER_HOST=unix://$SOCK"
    echo "export KUBECONFIG=$STABLE/kubeconfig"
    ;;
  *) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
