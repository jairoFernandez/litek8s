#!/usr/bin/env bash
# Run the corpus against real k3s (via k3d) to check that the cases themselves
# are correct: a reference cluster should pass all of them.
#
#   compat/baseline-k3d.sh [run.sh args...]     e.g. --tiers L0,L1
#
# k3d runs on any Docker API: Docker, OrbStack, or litek8s's own
# (DOCKER_HOST=unix://$HOME/.litek8s/docker.sock, docs/develop-without-docker.md).
# Without any, compat/baseline-k3s-vm.sh runs k3s in a qemu VM instead.
#
# Uses its own kubeconfig file, so the user's current context is untouched.
# traefik and metrics-server are disabled because L4 installs both via Helm.
set -euo pipefail
cluster=litek8s-compat
dir=$(cd "$(dirname "$0")" && pwd)
export KUBECONFIG="$dir/.work/k3d-kubeconfig"
mkdir -p "$dir/.work"

if ! k3d cluster list "$cluster" >/dev/null 2>&1; then
    k3d cluster create "$cluster" --agents 1 --wait \
        --k3s-arg '--disable=traefik,metrics-server@server:0' \
        --kubeconfig-update-default=false
fi
k3d kubeconfig get "$cluster" >"$KUBECONFIG"
"$dir/run.sh" --target k3d-baseline "$@"
echo "cluster left running; delete with: k3d cluster delete $cluster"
