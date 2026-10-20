#!/usr/bin/env bash
# A pinned litek8s for developing litek8s itself: `litek8s dev` (the cluster
# and the Docker API that cargo test, the corpus and the Linux builds use)
# runs from a known-good revision, never from the checkout being edited.
# See docs/develop-without-docker.md.
#
#   dev/stable.sh install [rev]   build <rev> (default HEAD) into $LITEK8S_STABLE
#   dev/stable.sh up [dev up args] | down | status | reset
#   dev/stable.sh env             eval "$(dev/stable.sh env)": DOCKER_HOST, KUBECONFIG
#
# LITEK8S_STABLE (default ~/.litek8s/stable) holds bin/litek8s, vm/{Image,root.img,initrd.gz},
# REVISION, and is the pinned litek8s's LITEK8S_HOME: its own dev VM, cluster,
# docker.sock and kubeconfig (contexts lkstable-<cluster>), apart from an
# installed litek8s and from dev/lk. `install` builds the macOS binary with
# this machine's cargo (target dir: target/stable-host of this checkout) and
# the VM's Linux binary in a rust:alpine container on whatever Docker API is
# current: the running stable cluster's when there is one, else DOCKER_HOST /
# the docker context (the very first install, e.g. OrbStack). Its cargo cache
# stays in the Docker volume litek8s-guest-build.
#
# The same as `dev/lk stable <cmd>`.
set -euo pipefail
case "${1:-}" in
  install | up | down | status | reset | env) exec "$(dirname "$0")/lk" stable "$@" ;;
  *) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
