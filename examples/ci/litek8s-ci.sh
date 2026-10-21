#!/bin/sh
# A litek8s cluster (and a Docker API) for any Linux CI job: GitLab CI,
# Jenkins, Buildkite, CircleCI's machine executor, a plain VM.
#
#   eval "$(sh examples/ci/litek8s-ci.sh)"     # then kubectl, helm, docker
#
#   LITEK8S_VERSION=v0.2.1   a release (default: the latest)
#   LITEK8S_NODES=3          nodes (default 1)
#
# As a user with sudo it starts the rootless cluster; as root (a job in a
# privileged container, as GitLab's Docker executor on GitLab.com runs them)
# the same cluster without a user namespace. Progress goes to stderr; stdout
# is the `export` lines for KUBECONFIG and DOCKER_HOST.
set -eu
exec 3>&1 1>&2

sudo=""
[ "$(id -u)" = 0 ] || sudo=sudo
if command -v apt-get >/dev/null 2>&1; then
  pkgs="crun slirp4netns iptables iproute2 curl ca-certificates"
  [ -n "$sudo" ] && pkgs="$pkgs uidmap"
  $sudo env DEBIAN_FRONTEND=noninteractive apt-get update -qq
  # shellcheck disable=SC2086 # a list of packages
  $sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $pkgs >/dev/null
fi

curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh |
  $sudo env LITEK8S_INSTALL_DIR=/usr/local/bin LITEK8S_VERSION="${LITEK8S_VERSION:-}" sh
# Ubuntu >= 23.10: allow user namespaces for litek8s (AppArmor).
[ -z "$sudo" ] || sudo litek8s dev setup

# In a container the home directory is on the container's overlayfs, which
# cannot hold the overlay mounts of litek8s's containers (as for Docker in
# Docker): keep litek8s's data on a tmpfs there.
export_data=""
if [ -z "$sudo" ] && [ "$(stat -f -c %T "$HOME" 2>/dev/null)" = overlayfs ]; then
  mkdir -p /var/lib/litek8s-ci
  mountpoint -q /var/lib/litek8s-ci || mount -t tmpfs tmpfs /var/lib/litek8s-ci
  export XDG_DATA_HOME=/var/lib/litek8s-ci
  export_data=" XDG_DATA_HOME=/var/lib/litek8s-ci"
fi

# pasta, litek8s's default, drops TCP flows now and then on some cloud CI
# machines (docs/ci.md); slirp4netns did not.
export LITEK8S_DEV_NETWORK=slirp4netns
if [ "$(litek8s --version)" = "litek8s 0.2.0" ] && command -v pasta >/dev/null 2>&1; then
  echo "warning: litek8s 0.2.0 uses pasta whenever it is installed (LITEK8S_DEV_NETWORK came in 0.2.1); on a throwaway CI machine remove it first (apt-get remove passt)"
fi
# 3>&-: the cluster's processes outlive this script and must not hold the
# pipe `eval "$(...)"` reads to its end.
litek8s dev up --name ci --nodes "${LITEK8S_NODES:-1}" 3>&-

kubeconfig="${KUBECONFIG:-$HOME/.kube/config}"
socket="$(litek8s dev status --name ci -o json | sed -n 's/.*"docker_socket": "\(.*\)".*/\1/p')"
echo "export KUBECONFIG='${kubeconfig%%:*}' DOCKER_HOST='unix://$socket' DOCKER_BUILDKIT=0 COMPOSE_BAKE=false LITEK8S_DEV_NETWORK=slirp4netns$export_data" >&3
