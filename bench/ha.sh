#!/usr/bin/env bash
# A highly available control plane on local VMs (bench/vm.sh), litek8s or
# k3s with embedded etcd, for the HA benchmark (bench/HA.md):
#
#   lk1..lk3  API servers (each also a node), 2 vCPU / 2 GiB
#   lk4       an agent node, 2 vCPU / 2 GiB
#   lk5       the client: `litek8s lb` on 127.0.0.1:6443 in front of the three
#             servers, kubectl, the compat corpus and the conformance suite
#             run here (it is on the VMs' LAN), 4 vCPU / 4 GiB
#
#   bench/ha.sh up litek8s|k3s     boot, install, start; wait for 4 Ready nodes
#   bench/ha.sh push               new litek8s binary everywhere, restart
#   bench/ha.sh leader             name of the leader (Raft or etcd)
#   bench/ha.sh kill-leader        SIGKILL the leader's server process (systemd restarts it)
#   bench/ha.sh partition <vm> <secs>  cut <vm> off from the other servers (iptables)
#   bench/ha.sh nemesis <every> <for>  kill-leader every <every> s for <for> s
#   bench/ha.sh mem                PSS of each server process (MiB)
#   bench/ha.sh run <cmd...>       run a command on lk5 (KUBECONFIG set)
#   bench/ha.sh down               stop every VM
#
# Env: LITEK8S_BIN (linux binary, default target/release/litek8s),
# K3S_VERSION (default v1.33.4+k3s1), HA_SERVER_MEM/CPUS, HA_RUNNER_MEM/CPUS.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
STATE="$WORK/ha"
mkdir -p "$STATE"
K3S_VERSION="${K3S_VERSION:-v1.33.4+k3s1}"

log() { echo "[ha $(date +%H:%M:%S)] $*" >&2; }
ssh_port() { echo $((2400 + $1)); }
lan_ip() { echo "192.168.77.$((10 + $1))"; }
servers_csv() { echo "$(lan_ip 1):6443,$(lan_ip 2):6443,$(lan_ip 3):6443"; }
kind() { cat "$STATE/kind" 2>/dev/null || echo litek8s; }
binary() { echo "${LITEK8S_BIN:-$ROOT/target/release/litek8s}"; }

r() { "$VM" ssh "$@"; }
rroot() { local vm="$1"; shift; "$VM" ssh "$vm" "sudo bash -c $(printf '%q' "$*")"; }

install_litek8s() { # vm
  local vm="$1"
  "$VM" scp "$vm" "$(binary)" /tmp/litek8s
  rroot "$vm" 'install -m 0755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s'
}

base_packages() { # vm
  rroot "$1" 'command -v crun >/dev/null && command -v iptables >/dev/null || { apt-get update -qq >/dev/null; DEBIAN_FRONTEND=noninteractive apt-get install -y -qq crun iptables >/dev/null; }'
}

unit() { # name description exec [envfile]
  cat <<EOF
[Unit]
Description=$2
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
${4:+EnvironmentFile=$4}
ExecStart=$3
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
}

start_unit() { # vm name unit-text
  echo "$3" | r "$1" "sudo tee /etc/systemd/system/$2.service >/dev/null"
  rroot "$1" "systemctl daemon-reload && systemctl enable $2 >/dev/null 2>&1; systemctl restart $2"
}

token_env() { # vm
  printf 'LITEK8S_TOKEN=%s\n' "$(cat "$STATE/token")" | r "$1" 'sudo mkdir -p /etc/litek8s && sudo tee /etc/litek8s/token.env >/dev/null && sudo chmod 600 /etc/litek8s/token.env'
}

up_litek8s() {
  local common="--listen 0.0.0.0:6443 --data-dir /var/lib/litek8s/apiserver --runtime crun"
  start_unit lk1 litek8s-apiserver "$(unit litek8s-apiserver 'litek8s API server' "/usr/local/bin/litek8s apiserver $common --node-ip $(lan_ip 1) --cluster-init")"
  log "waiting for lk1"
  for _ in $(seq 1 90); do r lk1 'sudo test -s /var/lib/litek8s/apiserver/join-token' 2>/dev/null && break; sleep 1; done
  r lk1 'sudo cat /var/lib/litek8s/apiserver/join-token' >"$STATE/token"
  for i in 2 3; do
    token_env "lk$i"
    start_unit "lk$i" litek8s-apiserver "$(unit litek8s-apiserver 'litek8s API server' "/usr/local/bin/litek8s apiserver $common --node-ip $(lan_ip "$i") --server https://$(lan_ip 1):6443 --token \${LITEK8S_TOKEN}" /etc/litek8s/token.env)"
  done
  token_env lk4
  start_unit lk4 litek8s-agent "$(unit litek8s-agent 'litek8s node agent' "/usr/local/bin/litek8s agent --join https://$(lan_ip 1):6443 --node-ip $(lan_ip 4) --node-dir /var/lib/litek8s/node-agent --runtime crun" /etc/litek8s/token.env)"
  start_unit lk5 litek8s-lb "$(unit litek8s-lb 'litek8s API load balancer' "/usr/local/bin/litek8s lb --listen 127.0.0.1:6443 --server http://$(lan_ip 1):6443 --server http://$(lan_ip 2):6443 --server http://$(lan_ip 3):6443")"
  cat <<EOF | r lk5 'cat > ~/kubeconfig'
apiVersion: v1
kind: Config
clusters:
- name: ha
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: ha
  context: {cluster: ha, user: ha}
current-context: ha
users:
- name: ha
  user: {}
EOF
}

up_k3s() {
  local flags="--disable=traefik,servicelb,metrics-server --write-kubeconfig-mode=644"
  rroot lk1 "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' K3S_TOKEN=lkha-bench-token sh -s - server --cluster-init --node-ip $(lan_ip 1) $flags >/dev/null"
  for i in 2 3; do
    rroot "lk$i" "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' K3S_TOKEN=lkha-bench-token sh -s - server --server https://$(lan_ip 1):6443 --node-ip $(lan_ip "$i") $flags >/dev/null"
  done
  rroot lk4 "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' K3S_TOKEN=lkha-bench-token K3S_URL=https://$(lan_ip 1):6443 sh -s - agent --node-ip $(lan_ip 4) >/dev/null"
  start_unit lk5 litek8s-lb "$(unit litek8s-lb 'API load balancer' "/usr/local/bin/litek8s lb --tcp-check --listen 127.0.0.1:6443 --server $(lan_ip 1):6443 --server $(lan_ip 2):6443 --server $(lan_ip 3):6443")"
  r lk1 'cat /etc/rancher/k3s/k3s.yaml' | sed 's#server: https://127.0.0.1:6443#server: https://127.0.0.1:6443#' | r lk5 'cat > ~/kubeconfig'
}

runner_tools() {
  rroot lk5 'command -v kubectl >/dev/null || { curl -fsSL -o /usr/local/bin/kubectl https://dl.k8s.io/release/v1.33.4/bin/linux/amd64/kubectl && chmod +x /usr/local/bin/kubectl; }'
  rroot lk5 'command -v helm >/dev/null || { curl -fsSL https://get.helm.sh/helm-v3.19.0-linux-amd64.tar.gz | tar xz -C /tmp && install -m 0755 /tmp/linux-amd64/helm /usr/local/bin/helm; }'
  rroot lk5 'command -v jq >/dev/null || { apt-get update -qq >/dev/null; DEBIAN_FRONTEND=noninteractive apt-get install -y -qq jq >/dev/null; }'
  # The corpus and the benchmark drivers.
  tar -C "$ROOT" -czf "$STATE/compat.tgz" compat/run.sh compat/lib.sh compat/cases compat/charts.txt compat/conformance.sh compat/conformance_summary.py compat/summary.py bench/node 2>/dev/null
  "$VM" scp lk5 "$STATE/compat.tgz" /tmp/compat.tgz
  r lk5 'mkdir -p ~/lk && tar -xzf /tmp/compat.tgz -C ~/lk'
}

cmd_up() {
  local k="${1:-litek8s}" i
  echo "$k" >"$STATE/kind"
  "$VM" key >/dev/null # the base image, once, before the parallel boots
  for i in 1 2 3 4; do
    "$VM" up "lk$i" "$(ssh_port "$i")" "${HA_SERVER_MEM:-2048}" "${HA_SERVER_CPUS:-2}" "$(lan_ip "$i")" &
  done
  "$VM" up lk5 "$(ssh_port 5)" "${HA_RUNNER_MEM:-4096}" "${HA_RUNNER_CPUS:-4}" "$(lan_ip 5)" &
  wait
  for i in 1 2 3 4 5; do
    { base_packages "lk$i"; install_litek8s "lk$i"; } &
  done
  wait
  # apt and scp sometimes fail when five VMs do it at once: check, retry.
  for i in 1 2 3 4 5; do
    for _ in 1 2 3; do
      r "lk$i" 'command -v crun >/dev/null && test -x /usr/local/bin/litek8s' && break
      log "retrying the install on lk$i"
      base_packages "lk$i" || true
      install_litek8s "lk$i" || true
    done
  done
  runner_tools
  if [[ "$k" == k3s ]]; then up_k3s; else up_litek8s; fi
  log "waiting for 4 Ready nodes"
  for _ in $(seq 1 120); do
    ready=$(cmd_run kubectl get nodes --no-headers 2>/dev/null | awk '$2=="Ready"' | wc -l | tr -d ' ')
    [[ "$ready" == 4 ]] && { cmd_run kubectl get nodes -o wide; return 0; }
    sleep 3
  done
  cmd_run kubectl get nodes -o wide || true
  log "not all nodes became Ready"; return 1
}

cmd_push() {
  local i
  for i in 1 2 3 4 5; do install_litek8s "lk$i" & done
  wait
  for i in 1 2 3; do rroot "lk$i" 'systemctl restart litek8s-apiserver'; sleep 3; done
  rroot lk4 'systemctl restart litek8s-agent'
  rroot lk5 'systemctl restart litek8s-lb'
}

cmd_run() { r lk5 "cd ~/lk && KUBECONFIG=\$HOME/kubeconfig $(printf '%q ' "$@")"; }

# The VM of the current leader.
cmd_leader() {
  if [[ "$(kind)" == k3s ]]; then
    local i
    for i in 1 2 3; do
      # etcd's leader: the member whose status says it leads.
      if r "lk$i" "sudo ETCDCTL_API=3 bash -c 'curl -s --cacert /var/lib/rancher/k3s/server/tls/etcd/server-ca.crt --cert /var/lib/rancher/k3s/server/tls/etcd/client.crt --key /var/lib/rancher/k3s/server/tls/etcd/client.key https://127.0.0.1:2379/v3/maintenance/status -X POST -d {}'" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("leader")==d["header"]["member_id"] else 1)' 2>/dev/null; then
        echo "lk$i"; return 0
      fi
    done
    return 1
  fi
  r lk5 '/usr/local/bin/litek8s server list --server http://127.0.0.1:6443' | awk '$3=="leader"{print $1}'
}

cmd_kill_leader() {
  local vm; vm="$(cmd_leader)"
  [[ -n "$vm" ]] || { log "no leader"; return 1; }
  if [[ "$(kind)" == k3s ]]; then
    rroot "$vm" 'systemctl kill -s KILL k3s'
  else
    rroot "$vm" 'systemctl kill -s KILL litek8s-apiserver'
  fi
  log "killed the leader on $vm"
  echo "$vm"
}

cmd_partition() { # vm secs
  local vm="$1" secs="$2" i rules=""
  for i in 1 2 3; do
    [[ "lk$i" == "$vm" ]] && continue
    rules+="iptables -I INPUT -s $(lan_ip "$i") -j DROP; iptables -I OUTPUT -d $(lan_ip "$i") -j DROP; "
  done
  rroot "$vm" "$rules"
  log "partitioned $vm from the other servers for ${secs}s"
  sleep "$secs"
  rroot "$vm" 'iptables -F INPUT; iptables -F OUTPUT'
  log "healed $vm"
}

# Kill the leader every <interval> seconds for <total> seconds; log when.
cmd_nemesis() {
  local interval="$1" total="$2" end
  end=$(( $(date +%s) + total ))
  while (( $(date +%s) + interval < end )); do
    sleep "$interval"
    echo "$(date +%s.%N) kill $(cmd_kill_leader 2>/dev/null || echo none)" >>"$STATE/nemesis.log"
  done
}

cmd_mem() {
  local i proc
  proc=$([[ "$(kind)" == k3s ]] && echo 'k3s server' || echo 'litek8s apiserver')
  for i in 1 2 3; do
    printf 'lk%s ' "$i"
    rroot "lk$i" "pid=\$(pgrep -f '$proc' | head -1); awk '/^Pss:/{s+=\$2} END{printf \"%.1f MiB\n\", s/1024}' /proc/\$pid/smaps_rollup"
  done
}

cmd_down() {
  local i
  for i in 1 2 3 4 5; do "$VM" down "lk$i"; done
  "$VM" lanhub-stop
  rm -rf "$STATE"
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  push) cmd_push ;;
  leader) cmd_leader ;;
  kill-leader) cmd_kill_leader ;;
  partition) shift; cmd_partition "$@" ;;
  nemesis) shift; cmd_nemesis "$@" ;;
  mem) cmd_mem ;;
  run) shift; cmd_run "$@" ;;
  down) cmd_down ;;
  *) sed -n '2,24p' "$0"; exit 2 ;;
esac
