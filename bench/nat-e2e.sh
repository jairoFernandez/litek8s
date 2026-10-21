#!/usr/bin/env bash
# End to end check of the overlay across NAT (docs/networking.md): three
# fresh Ubuntu 24.04 VMs (bench/vm.sh) on one LAN that stands for the
# Internet. lkn1 runs the API server and is a node with a public address
# (the relay); lkn2 and lkn3 are two homes: each runs its agent in a network
# namespace behind its own NAT (both homes use 10.99.0.0/24, like two home
# routers), so nothing reaches them from outside unless they send first.
#
# Phases, all through kubectl on lkn1:
#   1. relay:   the homes cannot reach each other (Linux NATs remap the port
#               of a flow that meets an unanswered packet from the other
#               side, so neither can punch through): lkn2 <-> lkn3 go through
#               lkn1; pod-to-pod and Services both ways, relayed bytes in the
#               metrics and `litek8s tree`
#   2. direct:  lkn3's router forwards UDP 51820 (the endpoint lkn1 saw it
#               at): the homes go direct, nothing is relayed
#   3. block:   that path breaks: back through lkn1, traffic recovers
#   4. unblock: direct again
#   5. the M1 (multi-node) and M2 (relay) tiers of the compat corpus, with
#      node power and path blocking provided by this script
#
#   bench/nat-e2e.sh                run it (VMs stay up on failure)
#   bench/nat-e2e.sh down           stop the VMs and the tunnel
#   bench/nat-e2e.sh phases         run the phases again on the VMs up
#   bench/nat-e2e.sh block A B      cut the path between nodes A and B
#   bench/nat-e2e.sh unblock A B
#   bench/nat-e2e.sh forward N on|off   port forward UDP 51820 on home N's router
#   bench/nat-e2e.sh kubeconfig     print the kubeconfig path
#
# Env: LITEK8S_BIN (a linux binary; default: bench/build-linux.sh),
# E2E_KEEP=1 keeps the VMs after a pass, E2E_PHASES (default "1 2 3 4 5"),
# E2E_SYMMETRIC=1 makes lkn3's NAT symmetric (random ports),
# E2E_KUBE_PORT (16472), BENCH_LAN_PORT (47750).
# On an x86_64 Linux host (omarchy) use BENCH_LAN_MCAST=1.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
STATE="$WORK/nat-e2e"
KPORT="${E2E_KUBE_PORT:-16472}"
export BENCH_LAN_PORT="${BENCH_LAN_PORT:-47750}"
KCFG="$STATE/kubeconfig"
HOMES=(lkn2 lkn3)
SERVER=lkn1
HOME_IP=10.99.0.2
mkdir -p "$STATE"

log() { echo "[nat-e2e $(date +%H:%M:%S)] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --kubeconfig "$KCFG" --request-timeout=15s "$@"; }
r() { "$VM" ssh "$@"; }
ssh_port() { echo $((2450 + ${1#lkn})); }
lan_ip() { echo "192.168.77.$((50 + ${1#lkn}))"; }
is_home() { [[ " ${HOMES[*]} " == *" $1 "* ]]; }

wait_until() { # secs description cmd...
  local secs=$1 what=$2 deadline
  shift 2
  deadline=$((SECONDS + secs))
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out after ${secs}s: $what"
    sleep 2
  done
}

binary() {
  if [[ -n "${LITEK8S_BIN:-}" ]]; then echo "$LITEK8S_BIN"; else "$HERE/build-linux.sh" "$([[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64)"; fi
}

install_node() { # name binary
  r "$1" 'command -v crun >/dev/null || { sudo apt-get update -qq >/dev/null; sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq crun iptables conntrack >/dev/null; }'
  "$VM" scp "$1" "$2" /tmp/litek8s
  r "$1" 'sudo install -m 0755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s'
}

# A home: the agent's network namespace behind a NAT on the VM, set up at
# every boot before the agent starts.
home_net() { # name
  local masq="-j MASQUERADE"
  [[ "${E2E_SYMMETRIC:-}" == 1 && "$1" == lkn3 ]] && masq="-j MASQUERADE --random-fully"
  cat <<EOF
#!/bin/sh
set -e
ip netns add home 2>/dev/null || true
ip link show home0 >/dev/null 2>&1 || ip link add home0 type veth peer name eth0 netns home
ip addr replace 10.99.0.1/24 dev home0
ip link set home0 up
ip -n home link set lo up
ip -n home addr replace $HOME_IP/24 dev eth0
ip -n home link set eth0 up
ip -n home route replace default via 10.99.0.1
sysctl -qw net.ipv4.ip_forward=1
iptables -t nat -C POSTROUTING -s 10.99.0.0/24 $masq 2>/dev/null || iptables -t nat -A POSTROUTING -s 10.99.0.0/24 $masq
iptables -N LKNAT-BLOCK 2>/dev/null || true
iptables -C FORWARD -j LKNAT-BLOCK 2>/dev/null || iptables -I FORWARD 1 -j LKNAT-BLOCK
EOF
}

units() { # name
  local name=$1
  if [[ "$name" == "$SERVER" ]]; then
    cat <<EOF
[Unit]
Description=litek8s API server (and local node)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
ExecStart=/usr/local/bin/litek8s apiserver --listen 0.0.0.0:6443 --data-dir /var/lib/litek8s/apiserver --node-ip $(lan_ip "$name") --runtime crun --node-monitor-grace-period 40s
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
  else
    cat <<EOF
[Unit]
Description=litek8s node agent (behind NAT)
After=network-online.target lk-home-net.service
Requires=lk-home-net.service
StartLimitIntervalSec=0

[Service]
EnvironmentFile=/etc/litek8s/agent.env
# ip netns exec also mounts the namespace's own /sys (crun looks up links
# there; systemd's NetworkNamespacePath= would leave the host's), without
# the cgroup tree: mount it again.
ExecStart=/usr/sbin/ip netns exec home /bin/sh -c 'mount -t cgroup2 cgroup2 /sys/fs/cgroup && exec /usr/local/bin/litek8s agent --join https://$(lan_ip "$SERVER"):6443 --node-ip $HOME_IP --node-dir /var/lib/litek8s/node-agent --runtime crun'
Restart=always
RestartSec=2
KillMode=process
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
  fi
}

home_unit() {
  cat <<'EOF'
[Unit]
Description=A home network behind NAT for the litek8s agent
After=network-online.target
Wants=network-online.target
Before=litek8s-agent.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/lk-home-net

[Install]
WantedBy=multi-user.target
EOF
}

start_server() {
  units "$SERVER" | r "$SERVER" 'sudo tee /etc/systemd/system/litek8s-apiserver.service >/dev/null'
  r "$SERVER" 'sudo systemctl daemon-reload && sudo systemctl enable --now litek8s-apiserver >/dev/null 2>&1; sudo systemctl restart litek8s-apiserver'
  for _ in $(seq 1 60); do
    r "$SERVER" 'test -s /var/lib/litek8s/apiserver/join-token' 2>/dev/null && break
    sleep 1
  done
  r "$SERVER" 'sudo cat /var/lib/litek8s/apiserver/join-token' >"$STATE/token"
}

start_home() { # name
  local name=$1
  home_net "$name" | r "$name" 'sudo tee /usr/local/sbin/lk-home-net >/dev/null && sudo chmod 755 /usr/local/sbin/lk-home-net'
  home_unit | r "$name" 'sudo tee /etc/systemd/system/lk-home-net.service >/dev/null'
  units "$name" | r "$name" 'sudo tee /etc/systemd/system/litek8s-agent.service >/dev/null'
  printf 'LITEK8S_TOKEN=%s\n' "$(cat "$STATE/token")" | r "$name" 'sudo mkdir -p /etc/litek8s && sudo tee /etc/litek8s/agent.env >/dev/null && sudo chmod 600 /etc/litek8s/agent.env'
  # Processes in the namespace cannot use systemd-resolved's 127.0.0.53.
  r "$name" 'sudo ln -sf /run/systemd/resolve/resolv.conf /etc/resolv.conf'
  r "$name" 'sudo systemctl daemon-reload && sudo systemctl enable lk-home-net litek8s-agent >/dev/null 2>&1; sudo systemctl restart lk-home-net litek8s-agent'
}

tunnel() {
  local pidfile="$STATE/tunnel.pid" key
  [[ -f "$pidfile" ]] && kill "$(cat "$pidfile")" 2>/dev/null || true
  key="$("$VM" key)"
  (
    while true; do
      ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
        -N -L "127.0.0.1:$KPORT:127.0.0.1:6443" -p "$(ssh_port "$SERVER")" bench@127.0.0.1 || true
      sleep 2
    done
  ) >/dev/null 2>&1 &
  echo $! >"$pidfile"
  cat >"$KCFG" <<EOF
apiVersion: v1
kind: Config
clusters:
- name: nat-e2e
  cluster: {server: "http://127.0.0.1:$KPORT"}
contexts:
- name: nat-e2e
  context: {cluster: nat-e2e, user: nat-e2e}
current-context: nat-e2e
users:
- name: nat-e2e
  user: {}
EOF
  wait_until 60 "the API server through the tunnel" k get --raw /healthz
}

# Cut (or restore) the path between two nodes: on each home, drop what its
# NAT forwards to or from the other node's public address.
cmd_block() { # a b
  local a=$1 b=$2 x y
  for x in "$a" "$b"; do
    y=$([[ "$x" == "$a" ]] && echo "$b" || echo "$a")
    is_home "$x" || fail "block: $x is not a home node"
    r "$x" "sudo iptables -C LKNAT-BLOCK -d $(lan_ip "$y") -j DROP 2>/dev/null || { sudo iptables -A LKNAT-BLOCK -d $(lan_ip "$y") -j DROP; sudo iptables -A LKNAT-BLOCK -s $(lan_ip "$y") -j DROP; }"
  done
}

# A port forward of UDP 51820 on a home's router to its node (on|off).
cmd_forward() { # node on|off
  local rule="PREROUTING -d $(lan_ip "$1") -p udp --dport 51820 -j DNAT --to-destination $HOME_IP:51820"
  if [[ "$2" == on ]]; then
    # NAT rules apply to new flows: drop the one the other home's attempts
    # keep alive (a router reloading its firewall does the same).
    r "$1" "sudo iptables -t nat -C $rule 2>/dev/null || sudo iptables -t nat -I $rule; sudo conntrack -D -p udp -d $(lan_ip "$1") --dport 51820 >/dev/null 2>&1 || true"
  else
    r "$1" "sudo iptables -t nat -D $rule 2>/dev/null || true"
  fi
}

cmd_unblock() { # a b
  local x
  for x in "$1" "$2"; do
    is_home "$x" && r "$x" 'sudo iptables -F LKNAT-BLOCK'
  done
}

# What node <a> reports for peer <b> (direct/unreachable) and its route.
peer_state() { k get node "$1" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-peers}' | python3 -c "import json,sys; print(json.load(sys.stdin).get('$2',{}).get('state',''))" 2>/dev/null; }
route() { k get node "$1" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}' | python3 -c "import json,sys; print(json.load(sys.stdin).get('$2',''))" 2>/dev/null; }
both_direct() { [[ "$(peer_state lkn2 lkn3)" == direct && "$(peer_state lkn3 lkn2)" == direct && "$(route lkn2 lkn3)" == direct && "$(route lkn3 lkn2)" == direct ]]; }
both_relayed() { [[ "$(route lkn2 lkn3)" == "via $SERVER" && "$(route lkn3 lkn2)" == "via $SERVER" ]]; }
relayed_bytes() { r "$SERVER" "sudo iptables -t filter -nvxL LITEK8S-OVERLAY | awk '\$3==\"ACCEPT\"{print \$2}'"; }

workload() {
  k create namespace nat >/dev/null 2>&1 || true
  for n in lkn2 lkn3; do
    k -n nat apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: on-$n, labels: {app: who, at: $n}}
spec:
  nodeSelector: {kubernetes.io/hostname: $n}
  containers:
  - name: httpd
    image: busybox:1.36
    command: [sh, -c, 'mkdir -p /www && hostname > /www/index.html && exec httpd -f -p 8080 -h /www']
---
apiVersion: v1
kind: Service
metadata: {name: svc-$n}
spec:
  selector: {at: $n}
  ports: [{port: 80, targetPort: 8080}]
EOF
  done
  k -n nat wait --for=condition=Ready pod/on-lkn2 pod/on-lkn3 --timeout=240s >/dev/null || fail "workload pods not Ready"
}

# Both ways, by pod IP, Service name (DNS) and ClusterIP.
traffic() {
  local from to ip
  for from in lkn2 lkn3; do
    to=$([[ "$from" == lkn2 ]] && echo lkn3 || echo lkn2)
    ip=$(k -n nat get pod "on-$to" -o jsonpath='{.status.podIP}')
    [[ "$(k -n nat exec "on-$from" -- wget -qO- -T 3 "http://$ip:8080/")" == "on-$to" ]] || return 1
    [[ "$(k -n nat exec "on-$from" -- wget -qO- -T 3 "http://svc-$to/")" == "on-$to" ]] || return 1
  done
}

# Seconds until traffic works again (polls every second).
recovery() { # max
  local start=$SECONDS
  until traffic >/dev/null 2>&1; do
    ((SECONDS - start < $1)) || { echo -1; return; }
    sleep 1
  done
  echo $((SECONDS - start))
}

phase_relay() {
  log "1. relay: the homes cannot reach each other (two NATs that remap ports), lkn1 relays"
  local before after t
  t=$SECONDS
  wait_until 300 "lkn2 <-> lkn3 routed via $SERVER" both_relayed
  log "   relayed after $((SECONDS - t))s"
  t=$(recovery 120)
  [[ "$t" -ge 0 ]] || fail "traffic between the homes (relayed)"
  before=$(relayed_bytes)
  for _ in 1 2 3; do traffic || fail "traffic between the homes (relayed)"; done
  after=$(relayed_bytes)
  ((after > before)) || fail "nothing relayed by $SERVER ($before -> $after bytes)"
  log "   pod IP + Service both ways; relayed through $SERVER: $before -> $after bytes"
  # The metrics API sees it too, and `litek8s tree` shows the route.
  wait_until 90 "relayed bytes in the metrics" bash -c "kubectl --kubeconfig '$KCFG' get --raw '/prometheus/api/v1/query?query=litek8s_overlay_relayed_bytes_total' | grep -q '\"$SERVER\"'"
  wait_until 90 "peer metrics show lkn3 relayed" bash -c "kubectl --kubeconfig '$KCFG' get --raw '/prometheus/api/v1/query?query=litek8s_overlay_peer_relayed%3D%3D1' | grep -q '\"lkn3\"'"
  KUBECONFIG="$KCFG" "$BIN" tree --context nat-e2e --once | tee "$STATE/tree.txt" >&2
  grep -q "Node lkn2 .*overlay: lkn3 via $SERVER" "$STATE/tree.txt" || fail "litek8s tree does not show lkn2's route to lkn3"
}

phase_direct() {
  log "2. direct: lkn3's router forwards UDP 51820; the homes go direct"
  local before after t=$SECONDS
  cmd_forward lkn3 on
  wait_until 400 "lkn2 <-> lkn3 direct" both_direct
  log "   direct after $((SECONDS - t))s"
  echo "relayed_to_direct_secs=$((SECONDS - t))" >>"$STATE/results"
  traffic || fail "traffic between the homes (direct)"
  before=$(relayed_bytes)
  for _ in 1 2 3; do traffic || fail "traffic between the homes (direct)"; done
  after=$(relayed_bytes)
  ((after == before)) || fail "still relaying with a direct path ($before -> $after bytes)"
  log "   traffic ok, nothing relayed"
}

phase_block() {
  log "3. block: the direct path between the homes breaks"
  local t
  cmd_block lkn2 lkn3
  t=$(recovery 240)
  [[ "$t" -ge 0 ]] || fail "traffic did not come back after the path was cut"
  log "   traffic back after ${t}s"
  echo "block_recovery_secs=$t" >>"$STATE/results"
  wait_until 120 "lkn2 <-> lkn3 routed via $SERVER" both_relayed
  traffic || fail "traffic between the homes (relayed again)"
}

phase_unblock() {
  log "4. unblock: the direct path is back"
  local before after t=$SECONDS
  cmd_unblock lkn2 lkn3
  wait_until 400 "lkn2 <-> lkn3 direct again" both_direct
  log "   direct again after $((SECONDS - t))s"
  echo "unblock_direct_secs=$((SECONDS - t))" >>"$STATE/results"
  before=$(relayed_bytes)
  for _ in 1 2 3; do traffic || fail "traffic between the homes (direct again)"; done
  after=$(relayed_bytes)
  ((after == before)) || fail "still relaying after the direct path came back ($before -> $after bytes)"
}

phase_corpus() {
  log "5. compat corpus M1 + M2 across the NATs"
  local self="$HERE/nat-e2e.sh"
  (
    cd "$ROOT"
    KUBECONFIG="$KCFG" COMPAT_NODE_KILL="$VM kill" COMPAT_NODE_START="$VM start" \
      COMPAT_PATH_BLOCK="$self block" COMPAT_PATH_UNBLOCK="$self unblock" \
      compat/run.sh --tiers M1,M2 --target litek8s-nat
  ) 2>&1 | tee "$STATE/corpus.log" >&2
  grep -E '^M[12] +[a-z_]+ +pass' "$STATE/corpus.log" >/dev/null || fail "no compat case ran (see $STATE/corpus.log)"
  if grep -E '^M[12] +[a-z_]+ +(fail|timeout)' "$STATE/corpus.log" >&2; then fail "compat cases failed (see $STATE/corpus.log)"; fi
}

cmd_run() {
  local n
  : >"$STATE/results"
  "$VM" key >/dev/null # the base image, once
  for n in lkn1 lkn2 lkn3; do
    "$VM" up "$n" "$(ssh_port "$n")" "${E2E_MEM:-1536}" 2 "$(lan_ip "$n")" &
  done
  wait
  BIN="$(binary)"
  for n in lkn1 lkn2 lkn3; do install_node "$n" "$BIN" & done
  wait
  start_server
  for n in "${HOMES[@]}"; do start_home "$n"; done
  tunnel
  wait_until 180 "3 Ready nodes" bash -c "[[ \$(kubectl --kubeconfig '$KCFG' get nodes --no-headers 2>/dev/null | awk '\$2==\"Ready\"' | wc -l) == 3 ]]"
  k get nodes -o wide >&2
  run_phases
  [[ "${E2E_KEEP:-}" == 1 ]] || cmd_down
}

# The phases against the VMs already up (bench/nat-e2e.sh phases).
run_phases() {
  [[ -n "${BIN:-}" ]] || BIN="$(binary)"
  workload
  for p in ${E2E_PHASES:-1 2 3 4 5}; do
    case "$p" in
      1) phase_relay ;;
      2) phase_direct ;;
      3) phase_block ;;
      4) phase_unblock ;;
      5) phase_corpus ;;
    esac
  done
  log "PASS"
  cat "$STATE/results" >&2
}

cmd_down() {
  [[ -f "$STATE/tunnel.pid" ]] && { pkill -P "$(cat "$STATE/tunnel.pid")" 2>/dev/null || true; kill "$(cat "$STATE/tunnel.pid")" 2>/dev/null || true; rm -f "$STATE/tunnel.pid"; }
  for n in lkn1 lkn2 lkn3; do "$VM" down "$n" || true; done
  "$VM" lanhub-stop || true
}

case "${1:-run}" in
  run) cmd_run ;;
  down) cmd_down ;;
  block) cmd_block "$2" "$3" ;;
  unblock) cmd_unblock "$2" "$3" ;;
  forward) cmd_forward "$2" "$3" ;;
  tunnel) tunnel ;;
  phases) tunnel; run_phases ;;
  kubeconfig) echo "$KCFG" ;;
  *) sed -n '2,31p' "$0"; exit 2 ;;
esac
