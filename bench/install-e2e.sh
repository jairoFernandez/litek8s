#!/usr/bin/env bash
# End to end check of the one-line server install: two fresh Ubuntu 24.04
# VMs (bench/vm.sh) go from nothing to a server and a joined agent with one
# command each (`install.sh server` / `install.sh agent`, served from a local
# HTTP server as if it were a GitHub release), then pods have to survive
# `systemctl restart` of both services and come back after a reboot of both
# machines. Ends with `litek8s uninstall --purge` on the agent.
#
#   bench/install-e2e.sh          run it (VMs stay up on failure)
#   bench/install-e2e.sh down     stop the VMs, the HTTP server and the tunnel
#
# Env: LITEK8S_BIN (a linux binary; default: bench/build-linux.sh), E2E_KEEP=1
# keeps the VMs after a pass, E2E_HTTP_PORT (18471), E2E_KUBE_PORT (16471).
# kubectl always runs with this script's own kubeconfig.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
STATE="$WORK/install-e2e"
HTTP_PORT="${E2E_HTTP_PORT:-18471}"
KPORT="${E2E_KUBE_PORT:-16471}"
export BENCH_LAN_PORT="${BENCH_LAN_PORT:-47740}"
SERVER=lki1 AGENT=lki2
SERVER_IP=192.168.77.41 AGENT_IP=192.168.77.42
VERSION=v0.0.0-e2e
KCFG="$STATE/kubeconfig"
mkdir -p "$STATE"

log() { echo "[install-e2e] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --kubeconfig "$KCFG" --request-timeout=10s "$@"; }
r() { "$VM" ssh "$@"; }

arch() { [[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64; }
target() { [[ "$(arch)" == amd64 ]] && echo linux-x86_64 || echo linux-aarch64; }

# A release as install.sh expects it, plus install.sh itself.
package() {
  local bin dl="$STATE/dl" name
  name="litek8s-$VERSION-$(target)"
  bin="${LITEK8S_BIN:-$("$HERE/build-linux.sh" "$(arch)")}"
  rm -rf "$dl"; mkdir -p "$dl/$name/bin"
  cp "$bin" "$dl/$name/bin/litek8s"
  COPYFILE_DISABLE=1 tar -C "$dl" -czf "$dl/$name.tar.gz" "$name"
  rm -rf "${dl:?}/$name"
  (cd "$dl" && shasum -a 256 "$name.tar.gz" >SHA256SUMS)
  cp "$ROOT/install.sh" "$dl/install.sh"
}

serve() {
  stop_pid http
  nohup python3 -m http.server --bind 127.0.0.1 "$HTTP_PORT" --directory "$STATE/dl" >"$STATE/http.log" 2>&1 &
  echo $! >"$STATE/http.pid"
}

stop_pid() { local f="$STATE/$1.pid"; [[ -f "$f" ]] && { kill "$(cat "$f")" 2>/dev/null || true; rm -f "$f"; }; return 0; }

# The one command a user runs on each machine (qemu's user network reaches
# the Mac's loopback at 10.0.2.2).
one_liner() { # vm args...
  local vm="$1"; shift
  r "$vm" "curl -fsSL http://10.0.2.2:$HTTP_PORT/install.sh | LITEK8S_VERSION=$VERSION LITEK8S_DOWNLOAD_URL=http://10.0.2.2:$HTTP_PORT sh -s -- $*"
}

tunnel() {
  local key; key="$("$VM" key)"
  stop_pid tunnel
  (
    while true; do
      ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
        -N -L "127.0.0.1:$KPORT:127.0.0.1:6443" -p "$(cat "$WORK/vms/$SERVER/port")" bench@127.0.0.1 || true
      sleep 2
    done
  ) >/dev/null 2>&1 &
  echo $! >"$STATE/tunnel.pid"
  r "$SERVER" 'sudo cat /etc/litek8s/kubeconfig.yaml' | sed "s#http://127.0.0.1:6443#http://127.0.0.1:$KPORT#" >"$KCFG"
}

wait_for() { # seconds description command...
  local secs="$1" what="$2"; shift 2
  local deadline=$((SECONDS + secs))
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out after ${secs}s waiting for $what"
    sleep 2
  done
}

nodes_ready() {
  [[ "$(k get nodes -o jsonpath='{range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' | grep -c True)" == 2 ]]
}

deploy_available() {
  [[ "$(k get deploy web -o jsonpath='{.status.availableReplicas}')" == 2 ]]
}

# name uid node restartCount startedAt, one pod per line.
pods() {
  k get pods -l app=web -o jsonpath='{range .items[*]}{.metadata.name} {.metadata.uid} {.spec.nodeName} {.status.containerStatuses[0].restartCount} {.status.containerStatuses[0].state.running.startedAt}{"\n"}{end}' | sort
}

pod_ip_on() { # node
  k get pods -l app=web -o jsonpath="{range .items[?(@.spec.nodeName==\"$1\")]}{.status.podIP}{\"\\n\"}{end}" | head -1
}

booted_again() { # vm old-boot-id
  local id; id="$(r "$1" cat /proc/sys/kernel/random/boot_id)" && [[ -n "$id" && "$id" != "$2" ]]
}

serves() { # vm ip
  r "$1" "curl -fsS -m 5 http://$2/ | grep -q 'Welcome to nginx'"
}

# The pods' containers keep running across a restart of the services.
check_pods_survived() { # before
  local after
  wait_for 120 "both nodes Ready" nodes_ready
  wait_for 120 "the deployment" deploy_available
  after="$(pods)"
  [[ "$after" == "$1" ]] || fail "pods changed across the restart:\nbefore:\n$1\nafter:\n$after"
}

cmd_run() {
  command -v kubectl >/dev/null || fail "needs kubectl"
  log "packaging a release ($VERSION)"
  package
  serve
  log "booting $SERVER and $AGENT (fresh Ubuntu 24.04)"
  "$VM" up "$SERVER" 2451 2048 2 "$SERVER_IP"
  "$VM" up "$AGENT" 2452 2048 2 "$AGENT_IP"

  log "command 1 on $SERVER: install.sh server"
  # The VMs' default route is qemu's NAT (10.0.2.15 on both); their LAN is
  # the second NIC, hence --node-ip.
  one_liner "$SERVER" "server --node-ip $SERVER_IP" | tee "$STATE/server-install.log"
  grep -q "litek8s server is ready" "$STATE/server-install.log" || fail "server install did not finish"
  local join; join="$(grep -o 'sudo sh -s -- agent --join [^ ]* --token [^ ]*' "$STATE/server-install.log" | head -1 | sed 's/^sudo sh -s -- //')"
  [[ -n "$join" ]] || fail "the server printed no join command"

  log "command 2 on $AGENT: install.sh $join --node-ip $AGENT_IP"
  one_liner "$AGENT" "$join --node-ip $AGENT_IP" | tee "$STATE/agent-install.log"
  grep -q "is Ready" "$STATE/agent-install.log" || fail "agent install did not finish"

  log "secrets stay out of units and command lines"
  local token="${join##* }"
  r "$SERVER" "sudo stat -c '%a %U' /etc/litek8s/kubeconfig.yaml" | grep -qx '600 root' || fail "kubeconfig is not 0600 root"
  r "$AGENT" "sudo stat -c '%a %U' /etc/litek8s/agent.env" | grep -qx '600 root' || fail "agent.env is not 0600 root"
  r "$AGENT" "sudo grep -q -- '$token' /etc/systemd/system/litek8s-agent.service" && fail "the token is in the agent unit"
  r "$AGENT" "sudo cat /proc/\$(systemctl show -p MainPID --value litek8s-agent)/cmdline | tr '\\0' ' '" | grep -q -- "$token" && fail "the token is on the agent's command line"
  [[ "$(r "$AGENT" 'systemctl show -p KillMode --value litek8s-agent; systemctl show -p Restart --value litek8s-agent' | tr '\n' ' ')" == "process always " ]] ||
    fail "agent unit lacks KillMode=process / Restart=always"

  tunnel
  wait_for 60 "the API through the tunnel" k get --raw /readyz
  wait_for 120 "both nodes Ready" nodes_ready
  k get nodes -o wide

  log "a deployment over both nodes"
  k apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: 2
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: nginx
        image: nginx:1.27-alpine
        ports: [{containerPort: 80}]
EOF
  wait_for 300 "the deployment" deploy_available
  local before; before="$(pods)"
  echo "$before"
  [[ "$(awk '{print $3}' <<<"$before" | sort -u | wc -l | tr -d ' ')" == 2 ]] || fail "the replicas did not spread over both nodes"
  local agent_pod; agent_pod="$(pod_ip_on "$AGENT")"
  wait_for 60 "$SERVER reaching the pod on $AGENT over the overlay" serves "$SERVER" "$agent_pod"

  log "systemctl restart of both services keeps the pods"
  r "$SERVER" 'sudo systemctl restart litek8s-server'
  r "$AGENT" 'sudo systemctl restart litek8s-agent'
  sleep 5
  wait_for 60 "the API after the restart" k get --raw /readyz
  check_pods_survived "$before"
  serves "$SERVER" "$(pod_ip_on "$AGENT")" || fail "pod on $AGENT unreachable after the restart"

  log "re-running install changes the flags (token from the saved env file)"
  r "$AGENT" "sudo litek8s install agent --join https://$SERVER_IP:6443 --node-ip $AGENT_IP --node-grace-period 2m" | tee "$STATE/agent-reinstall.log"
  r "$AGENT" 'grep -q -- "--node-grace-period 2m" /etc/systemd/system/litek8s-agent.service' || fail "re-install did not update the unit"
  check_pods_survived "$before"

  log "reboot both machines"
  local boot1 boot2
  boot1="$(r "$SERVER" cat /proc/sys/kernel/random/boot_id)"
  boot2="$(r "$AGENT" cat /proc/sys/kernel/random/boot_id)"
  r "$SERVER" 'sudo systemctl reboot' || true
  r "$AGENT" 'sudo systemctl reboot' || true
  sleep 15
  wait_for 300 "$SERVER back from its reboot" booted_again "$SERVER" "$boot1"
  wait_for 300 "$AGENT back from its reboot" booted_again "$AGENT" "$boot2"
  r "$SERVER" 'systemctl is-enabled litek8s-server && systemctl is-active litek8s-server'
  r "$AGENT" 'systemctl is-enabled litek8s-agent && systemctl is-active litek8s-agent'
  wait_for 120 "the API after the reboot" k get --raw /readyz
  wait_for 300 "both nodes Ready after the reboot" nodes_ready
  wait_for 300 "the deployment after the reboot" deploy_available
  pods
  wait_for 120 "the pod on $AGENT serving after the reboot" serves "$SERVER" "$(pod_ip_on "$AGENT")"

  log "uninstall --purge on $AGENT"
  r "$AGENT" 'sudo litek8s uninstall --purge'
  r "$AGENT" 'test ! -e /etc/systemd/system/litek8s-agent.service && test ! -e /var/lib/litek8s && test ! -e /etc/litek8s' || fail "uninstall left files"
  r "$AGENT" '! pgrep -x litek8s' || fail "litek8s processes left after uninstall"
  r "$AGENT" '! ip link show litek8s0 2>/dev/null && ! ip link show litek8s-wg 2>/dev/null' || fail "uninstall left network links"
  r "$AGENT" '! sudo iptables-save | grep -q LITEK8S' || fail "uninstall left iptables rules"
  r "$AGENT" 'test ! -e /sys/fs/cgroup/litek8s' || fail "uninstall left the pods' cgroups"

  log "PASS"
  [[ "${E2E_KEEP:-}" == 1 ]] || cmd_down
}

cmd_down() {
  stop_pid tunnel
  stop_pid http
  "$VM" down "$SERVER" || true
  "$VM" down "$AGENT" || true
}

case "${1:-run}" in
  run) cmd_run ;;
  down) cmd_down ;;
  *) echo "usage: $0 [run|down]" >&2; exit 2 ;;
esac
