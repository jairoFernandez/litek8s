#!/usr/bin/env bash
# The corpus's reference run on real k3s without Docker: k3s in a throwaway
# bench/vm.sh VM (Ubuntu 24.04 under qemu, HVF on macOS / KVM on Linux), its
# API reached through an SSH tunnel on 127.0.0.1. Same k3s version and
# options as compat/baseline-k3d.sh, one node instead of k3d's server + agent.
#
#   compat/baseline-k3s-vm.sh [run.sh args...]     e.g. --tiers L0,L1
#   compat/baseline-k3s-vm.sh --down               stop the tunnel and delete the VM
#
#   K3S_VM=k3sref  K3S_VM_SSH_PORT=2440  K3S_VM_MEM=4096 (MiB)  K3S_VM_CPUS=4
#   K3S_API_PORT=26443  K3S_VERSION=v1.33.4+k3s1
#
# Uses its own kubeconfig file, so the user's current context is untouched.
# The VM and tunnel are left running for reruns.
set -euo pipefail
dir=$(cd "$(dirname "$0")" && pwd)
vm_sh="$dir/../bench/vm.sh"
vm=${K3S_VM:-k3sref}
api_port=${K3S_API_PORT:-26443}
work="$dir/.work"
tunnel_pid="$work/k3s-vm-tunnel.pid"
export KUBECONFIG="$work/k3s-vm-kubeconfig"
mkdir -p "$work"

stop_tunnel() {
    [[ -f "$tunnel_pid" ]] && kill "$(cat "$tunnel_pid")" 2>/dev/null || true
    rm -f "$tunnel_pid"
}

if [[ "${1:-}" == --down ]]; then
    stop_tunnel
    "$vm_sh" down "$vm"
    exit 0
fi

if ! "$vm_sh" ssh "$vm" true 2>/dev/null; then
    "$vm_sh" up "$vm" "${K3S_VM_SSH_PORT:-2440}" "${K3S_VM_MEM:-4096}" "${K3S_VM_CPUS:-4}"
fi
"$vm_sh" ssh "$vm" "command -v k3s >/dev/null || curl -sfL https://get.k3s.io | \
    INSTALL_K3S_VERSION='${K3S_VERSION:-v1.33.4+k3s1}' \
    INSTALL_K3S_EXEC='server --disable=traefik,metrics-server --write-kubeconfig-mode=644' sh -s - >/dev/null"
"$vm_sh" ssh "$vm" 'until [ -f /etc/rancher/k3s/k3s.yaml ]; do sleep 1; done; cat /etc/rancher/k3s/k3s.yaml' |
    sed "s#https://127.0.0.1:6443#https://127.0.0.1:$api_port#" >"$KUBECONFIG"

if ! { [[ -f "$tunnel_pid" ]] && kill -0 "$(cat "$tunnel_pid")" 2>/dev/null; }; then
    key=$("$vm_sh" key)
    port=$(cat "${BENCH_WORK:-$dir/../bench/.work}/vms/$vm/port")
    ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ServerAliveInterval=5 -o ExitOnForwardFailure=yes -N -L "127.0.0.1:$api_port:127.0.0.1:6443" \
        -p "$port" bench@127.0.0.1 &
    echo $! >"$tunnel_pid"
fi
for _ in $(seq 1 120); do
    kubectl get nodes 2>/dev/null | grep -q ' Ready ' && break
    sleep 2
done
kubectl get nodes

"$dir/run.sh" --target k3s-vm-baseline "$@"
echo "VM $vm left running; delete with: compat/baseline-k3s-vm.sh --down"
