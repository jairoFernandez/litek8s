#!/usr/bin/env bash
# A multi-node litek8s cluster on local VMs (bench/vm.sh): lk1 runs the API
# server (and is a node), lk2..lkN join it with `litek8s agent --join`. Pods
# run on crun; nodes reach each other on a private LAN (192.168.77.0/24) and
# pods over the WireGuard overlay. Every litek8s process is a systemd unit
# with Restart=always, so a VM that is killed and booted again rejoins.
#
#   bench/cluster.sh up [nodes]       boot lk1..lkN (default 3), install, start
#   bench/cluster.sh push             copy a fresh binary everywhere, restart
#   bench/cluster.sh tunnel           (re)open kubectl's tunnel to lk1:6443
#   bench/cluster.sh kubeconfig       print the kubeconfig path
#   bench/cluster.sh add <name> <ssh-port> <lan-ip>   join one more VM
#   bench/cluster.sh down             stop every VM of the cluster
#
# Env: CLUSTER_GRACE (node monitor grace, default 40s), LITEK8S_BIN (binary;
# default: bench/build-linux.sh arm64), CLUSTER_PORT (local tunnel, 16443).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
PORT="${CLUSTER_PORT:-16443}"
KUBECONFIG_OUT="$WORK/cluster.kubeconfig"
STATE="$WORK/cluster"
mkdir -p "$STATE"

log() { echo "[cluster] $*" >&2; }
ssh_port() { echo $((2400 + $1)); }
lan_ip() { echo "192.168.77.$((10 + $1))"; }

binary() {
  if [[ -n "${LITEK8S_BIN:-}" ]]; then echo "$LITEK8S_BIN"; else "$HERE/build-linux.sh" arm64; fi
}

install_node() { # name binary
  local name="$1" bin="$2"
  "$VM" ssh "$name" 'command -v crun >/dev/null || { sudo apt-get update -qq >/dev/null; sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq crun iptables >/dev/null; }'
  "$VM" scp "$name" "$bin" /tmp/litek8s
  "$VM" ssh "$name" 'sudo install -m 0755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s'
}

server_unit() {
  cat <<EOF
[Unit]
Description=litek8s API server (and local node)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
ExecStart=/usr/local/bin/litek8s apiserver --listen 0.0.0.0:6443 --data-dir /var/lib/litek8s/apiserver --node-ip $(lan_ip 1) --runtime crun --node-monitor-grace-period ${CLUSTER_GRACE:-40s}
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
}

agent_unit() { # index
  cat <<EOF
[Unit]
Description=litek8s node agent
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
EnvironmentFile=/etc/litek8s/agent.env
ExecStart=/usr/local/bin/litek8s agent --join https://$(lan_ip 1):6443 --node-ip $(lan_ip "$1") --node-dir /var/lib/litek8s/node-agent --runtime crun
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
}

start_server() {
  server_unit | "$VM" ssh lk1 'sudo tee /etc/systemd/system/litek8s-apiserver.service >/dev/null'
  "$VM" ssh lk1 'sudo systemctl daemon-reload && sudo systemctl enable --now litek8s-apiserver >/dev/null 2>&1; sudo systemctl restart litek8s-apiserver'
  log "waiting for the API server on lk1"
  for _ in $(seq 1 60); do
    if "$VM" ssh lk1 'test -s /var/lib/litek8s/apiserver/join-token' 2>/dev/null; then break; fi
    sleep 1
  done
  "$VM" ssh lk1 'sudo cat /var/lib/litek8s/apiserver/join-token' >"$STATE/token"
}

start_agent() { # index (name lk<index>)
  local i="$1" name="lk$1"
  agent_unit "$i" | "$VM" ssh "$name" 'sudo tee /etc/systemd/system/litek8s-agent.service >/dev/null'
  # shellcheck disable=SC2002
  printf 'LITEK8S_TOKEN=%s\n' "$(cat "$STATE/token")" | "$VM" ssh "$name" 'sudo mkdir -p /etc/litek8s && sudo tee /etc/litek8s/agent.env >/dev/null && sudo chmod 600 /etc/litek8s/agent.env'
  "$VM" ssh "$name" 'sudo systemctl daemon-reload && sudo systemctl enable litek8s-agent >/dev/null 2>&1; sudo systemctl restart litek8s-agent'
}

cmd_tunnel() {
  local pidfile="$STATE/tunnel.pid" key
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then kill "$(cat "$pidfile")" 2>/dev/null || true; fi
  key="$("$VM" key)"
  # A supervisor loop, so the tunnel survives lk1 reboots.
  (
    while true; do
      ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
        -N -L "127.0.0.1:$PORT:127.0.0.1:6443" -p "$(ssh_port 1)" bench@127.0.0.1 || true
      sleep 2
    done
  ) >/dev/null 2>&1 &
  echo $! >"$pidfile"
  cat >"$KUBECONFIG_OUT" <<EOF
apiVersion: v1
kind: Config
clusters:
- name: litek8s-vms
  cluster:
    server: http://127.0.0.1:$PORT
contexts:
- name: litek8s-vms
  context:
    cluster: litek8s-vms
    user: litek8s
current-context: litek8s-vms
users:
- name: litek8s
  user: {}
EOF
  for _ in $(seq 1 30); do
    KUBECONFIG="$KUBECONFIG_OUT" kubectl get --raw /healthz >/dev/null 2>&1 && return 0
    sleep 1
  done
  log "tunnel up but the API server does not answer yet"
}

cmd_up() {
  local n="${1:-3}" bin i
  echo "$n" >"$STATE/nodes"
  for i in $(seq 1 "$n"); do
    "$VM" up "lk$i" "$(ssh_port "$i")" "${CLUSTER_MEM:-1024}" "${CLUSTER_CPUS:-2}" "$(lan_ip "$i")" &
  done
  wait
  bin="$(binary)"
  for i in $(seq 1 "$n"); do install_node "lk$i" "$bin" & done
  wait
  start_server
  for i in $(seq 2 "$n"); do start_agent "$i"; done
  cmd_tunnel
  log "waiting for $n Ready nodes"
  for _ in $(seq 1 90); do
    ready=$(KUBECONFIG="$KUBECONFIG_OUT" kubectl get nodes --no-headers 2>/dev/null | awk '$2=="Ready"' | wc -l | tr -d ' ')
    [[ "$ready" == "$n" ]] && { KUBECONFIG="$KUBECONFIG_OUT" kubectl get nodes -o wide; return 0; }
    sleep 2
  done
  KUBECONFIG="$KUBECONFIG_OUT" kubectl get nodes -o wide || true
  log "not all nodes became Ready"; return 1
}

cmd_push() {
  local n bin i
  n="$(cat "$STATE/nodes")"
  bin="$(binary)"
  for i in $(seq 1 "$n"); do install_node "lk$i" "$bin" & done
  wait
  "$VM" ssh lk1 'sudo systemctl restart litek8s-apiserver'
  for i in $(seq 2 "$n"); do "$VM" ssh "lk$i" 'sudo systemctl restart litek8s-agent'; done
}

cmd_add() { # name ssh-port lan-ip: join an extra VM (index from its LAN IP)
  local name="$1" port="$2" ip="$3" i
  i=$(( ${ip##*.} - 10 ))
  [[ "$name" == "lk$i" ]] || { log "name must be lk$i for $ip"; return 1; }
  "$VM" up "$name" "$port" "${CLUSTER_MEM:-1024}" "${CLUSTER_CPUS:-2}" "$ip"
  install_node "$name" "$(binary)"
  start_agent "$i"
  echo $(( $(cat "$STATE/nodes") > i ? $(cat "$STATE/nodes") : i )) >"$STATE/nodes"
}

cmd_down() {
  local n i
  n="$(cat "$STATE/nodes" 2>/dev/null || echo 3)"
  [[ -f "$STATE/tunnel.pid" ]] && { pkill -P "$(cat "$STATE/tunnel.pid")" 2>/dev/null || true; kill "$(cat "$STATE/tunnel.pid")" 2>/dev/null || true; rm -f "$STATE/tunnel.pid"; }
  for i in $(seq 1 "$n"); do "$VM" down "lk$i"; done
  "$VM" lanhub-stop
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  push) cmd_push ;;
  tunnel) cmd_tunnel ;;
  kubeconfig) echo "$KUBECONFIG_OUT" ;;
  add) shift; cmd_add "$@" ;;
  down) cmd_down ;;
  *) sed -n '2,17p' "$0"; exit 2 ;;
esac
