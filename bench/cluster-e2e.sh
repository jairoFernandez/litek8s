#!/usr/bin/env bash
# End to end check of `litek8s cluster apply|status|delete` from this
# machine: three fresh Ubuntu 24.04 VMs (bench/vm.sh) become 1 server + 2
# agents over SSH. The first apply has the VMs download the binary as a
# release (a local HTTP server stands in for GitHub); then a re-apply with
# --binary changes nothing (no service restarted), pods run on all three
# nodes, an agent taken out of the file is drained, uninstalled and deleted
# with --prune, and `cluster delete --yes --purge` leaves the VMs clean.
#
#   bench/cluster-e2e.sh          run it (VMs stay up on failure)
#   bench/cluster-e2e.sh down     stop the VMs, the HTTP server and the tunnel
#
# Env: LITEK8S_BIN (a linux binary for the VMs; default: bench/build-linux.sh),
# LITEK8S_LOCAL (the litek8s that runs here; default: target/debug/litek8s),
# E2E_KEEP=1 keeps the VMs after a pass, E2E_HTTP_PORT (18481), E2E_KUBE_PORT
# (16481). Everything uses this script's own kubeconfig (KUBECONFIG).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
STATE="$WORK/cluster-e2e"
HTTP_PORT="${E2E_HTTP_PORT:-18481}"
KPORT="${E2E_KUBE_PORT:-16481}"
export BENCH_LAN_PORT="${BENCH_LAN_PORT:-47760}"
NAMES=(lkc1 lkc2 lkc3)
PORTS=(2471 2472 2473)
IPS=(192.168.77.61 192.168.77.62 192.168.77.63)
VERSION=v0.0.0-e2e
LOCAL="${LITEK8S_LOCAL:-$ROOT/target/debug/litek8s}"
export KUBECONFIG="$STATE/kubeconfig"
unset LITEK8S_HOME
mkdir -p "$STATE"

log() { echo "[cluster-e2e] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --context e2e --request-timeout=10s "$@"; }
r() { "$VM" ssh "$@"; }
lk() { "$LOCAL" "$@"; }

arch() { [[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64; }
target() { [[ "$(arch)" == amd64 ]] && echo linux-x86_64 || echo linux-aarch64; }

stop_pid() { local f="$STATE/$1.pid"; [[ -f "$f" ]] && { kill "$(cat "$f")" 2>/dev/null || true; rm -f "$f"; }; return 0; }

# A release as `install --ssh` downloads it.
package() {
  local dl="$STATE/dl" name
  name="litek8s-$VERSION-$(target)"
  rm -rf "$dl"; mkdir -p "$dl/$name/bin"
  cp "$BIN" "$dl/$name/bin/litek8s"
  COPYFILE_DISABLE=1 tar --no-xattrs -C "$dl" -czf "$dl/$name.tar.gz" "$name"
  rm -rf "${dl:?}/$name"
  (cd "$dl" && shasum -a 256 "$name.tar.gz" >SHA256SUMS)
  stop_pid http
  nohup python3 -m http.server --bind 127.0.0.1 "$HTTP_PORT" --directory "$dl" >"$STATE/http.log" 2>&1 &
  echo $! >"$STATE/http.pid"
}

# The API of lkc1 on 127.0.0.1:$KPORT here (the VMs' LAN is not reachable
# from this machine; the file says apiAddress: 127.0.0.1:$KPORT).
tunnel() {
  local key; key="$("$VM" key)"
  stop_pid tunnel
  (
    while true; do
      ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes \
        -N -L "127.0.0.1:$KPORT:127.0.0.1:6443" -p "${PORTS[0]}" bench@127.0.0.1 || true
      sleep 2
    done
  ) >/dev/null 2>&1 &
  echo $! >"$STATE/tunnel.pid"
}

# The cluster file: $1 = all (3 machines) or two (lkc3 left out, lkc2 relabeled).
cluster_file() {
  local key; key="$("$VM" key)"
  cat <<EOF
apiVersion: litek8s.dev/v1
kind: Cluster
metadata: {name: e2e}
apiAddress: 127.0.0.1:$KPORT
identityFile: $key
sshOptions: [StrictHostKeyChecking=no, UserKnownHostsFile=/dev/null, LogLevel=ERROR, IdentitiesOnly=yes]
servers:
  - {ssh: bench@127.0.0.1, sshPort: ${PORTS[0]}, name: lkc1, nodeIp: ${IPS[0]}}
agents:
EOF
  if [[ "$1" == all ]]; then
    cat <<EOF
  - {ssh: bench@127.0.0.1, sshPort: ${PORTS[1]}, nodeIp: ${IPS[1]}, labels: {disk: ssd}}
  - ssh: bench@127.0.0.1
    sshPort: ${PORTS[2]}
    name: lkc3
    nodeIp: ${IPS[2]}
    labels: {zone: b}
    taints: ['dedicated=batch:NoSchedule']
EOF
  else
    cat <<EOF
  - {ssh: bench@127.0.0.1, sshPort: ${PORTS[1]}, nodeIp: ${IPS[1]}, labels: {disk: nvme, zone: a}}
EOF
  fi
}

# "<unit> <MainPID> <ActiveEnterTimestampMonotonic>" of each machine's service.
services() {
  local i unit
  for i in 0 1 2; do
    unit=litek8s-agent; [[ $i == 0 ]] && unit=litek8s-server
    echo "${NAMES[$i]} $(r "${NAMES[$i]}" "systemctl show -p MainPID -p ActiveEnterTimestampMonotonic --value $unit" | tr '\n' ' ')"
  done
}

wait_for() { # seconds description command...
  local secs="$1" what="$2"; shift 2
  local deadline=$((SECONDS + secs))
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out after ${secs}s waiting for $what"
    sleep 2
  done
}

ready_nodes() { k get nodes -o jsonpath='{range .items[*]}{.metadata.name}={.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' | grep -c '=True'; }
nodes_ready() { [[ "$(ready_nodes)" == "$1" ]]; }
deploy_available() { [[ "$(k get deploy web -o jsonpath='{.status.availableReplicas}')" == "$1" ]]; }
pod_nodes() { k get pods -l app=web -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u; }
label() { k get node "$1" -o jsonpath="{.metadata.labels.$2}"; }
gone() { ! k get node "$1" >/dev/null 2>&1; }

cmd_run() {
  command -v kubectl >/dev/null || fail "needs kubectl"
  [[ -x "$LOCAL" ]] || fail "no $LOCAL (cargo build)"
  BIN="${LITEK8S_BIN:-$("$HERE/build-linux.sh" "$(arch)")}"
  rm -f "$KUBECONFIG"
  log "packaging $BIN as release $VERSION"
  package
  local i
  for i in 0 1 2; do
    log "booting ${NAMES[$i]} (fresh Ubuntu 24.04, ${IPS[$i]})"
    # `up` keeps a VM that is still running (from a failed run).
    "$VM" down "${NAMES[$i]}" >/dev/null 2>&1 || true
    "$VM" up "${NAMES[$i]}" "${PORTS[$i]}" 2048 2 "${IPS[$i]}"
  done
  tunnel
  cluster_file all >"$STATE/cluster.yaml"
  cluster_file two >"$STATE/cluster-two.yaml"
  cat "$STATE/cluster.yaml"

  log "apply 1: install everything, the VMs download release $VERSION"
  LITEK8S_DOWNLOAD_URL="http://10.0.2.2:$HTTP_PORT" lk cluster apply -f "$STATE/cluster.yaml" --release "$VERSION" | tee "$STATE/apply1.log"
  grep -q "downloading http://10.0.2.2:$HTTP_PORT/litek8s-$VERSION-" "$STATE/apply1.log" || fail "the binary was not downloaded as a release"
  grep -q "cluster e2e: .* change(s)" "$STATE/apply1.log" || fail "apply 1 reported no changes"
  wait_for 120 "3 nodes Ready" nodes_ready 3
  k get nodes -o wide --show-labels
  [[ "$(label lkc2 disk)" == ssd ]] || fail "lkc2 lacks disk=ssd"
  [[ "$(label lkc3 zone)" == b ]] || fail "lkc3 lacks zone=b"
  [[ "$(k get node lkc3 -o jsonpath='{.spec.taints[?(@.key=="dedicated")].effect}')" == NoSchedule ]] || fail "lkc3 lacks its taint"
  [[ "$(k get node lkc2 -o jsonpath='{.metadata.annotations.cluster\.litek8s\.dev/name}')" == e2e ]] || fail "lkc2 is not annotated with its cluster"
  r lkc2 'sudo test "$(stat -c %a /etc/litek8s/agent.env)" = 600' || fail "agent.env is not 0600"
  r lkc2 "! ls -d ~/.litek8s-install.* 2>/dev/null" || fail "the staging directory was left behind"

  log "apply 2 (--binary, the same build): nothing changes, nothing restarts"
  local before after
  before="$(services)"
  lk cluster apply -f "$STATE/cluster.yaml" --binary "$BIN" | tee "$STATE/apply2.log"
  grep -q "is up to date: nothing changed" "$STATE/apply2.log" || fail "re-apply was not a no-op"
  grep -q uploading "$STATE/apply2.log" && fail "re-apply uploaded the binary again"
  after="$(services)"
  [[ "$before" == "$after" ]] || fail "re-apply restarted services:\n$before\n$after"

  log "pods on all three nodes"
  k apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: 3
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      tolerations: [{key: dedicated, operator: Equal, value: batch, effect: NoSchedule}]
      containers:
      - name: nginx
        image: nginx:1.27-alpine
        ports: [{containerPort: 80}]
EOF
  wait_for 300 "the deployment" deploy_available 3
  k get pods -o wide
  [[ "$(pod_nodes | wc -l | tr -d ' ')" == 3 ]] || fail "the replicas are not on all three nodes: $(pod_nodes | tr '\n' ' ')"
  local ip3; ip3="$(k get pods -l app=web -o jsonpath='{range .items[?(@.spec.nodeName=="lkc3")]}{.status.podIP}{end}')"
  wait_for 60 "lkc1 reaching the pod on lkc3 over the overlay" r lkc1 "curl -fsS -m 5 http://$ip3/ | grep -q 'Welcome to nginx'"

  log "status"
  lk cluster status -f "$STATE/cluster.yaml" | tee "$STATE/status.log"
  [[ "$(grep -c ' active .* Ready ' "$STATE/status.log")" == 3 ]] || fail "status does not show 3 active, Ready machines"

  log "apply without lkc3 (and lkc2 relabeled): lkc3 stays until --prune"
  lk cluster apply -f "$STATE/cluster-two.yaml" --binary "$BIN" 2>&1 | tee "$STATE/apply3.log"
  grep -q "agent lkc3 is no longer in the file" "$STATE/apply3.log" || fail "apply did not report lkc3 as left out"
  grep -q "node lkc2: label disk=nvme, label zone=a" "$STATE/apply3.log" || fail "apply did not relabel lkc2"
  k get node lkc3 >/dev/null || fail "lkc3 was removed without --prune"
  [[ "$(label lkc2 disk)" == nvme ]] || fail "lkc2 not relabeled"

  log "apply --prune: lkc3 is drained, uninstalled and deleted"
  lk cluster apply -f "$STATE/cluster-two.yaml" --binary "$BIN" --prune | tee "$STATE/apply4.log"
  grep -q "agent lkc3 drained, uninstalled and deleted" "$STATE/apply4.log" || fail "prune did not remove lkc3"
  wait_for 30 "node lkc3 gone" gone lkc3
  r lkc3 'test ! -e /etc/systemd/system/litek8s-agent.service && ! pgrep -x litek8s' || fail "lkc3 still runs litek8s"
  wait_for 180 "the deployment back on two nodes" deploy_available 3
  pod_nodes | grep -q lkc3 && fail "pods still on lkc3"
  lk cluster apply -f "$STATE/cluster-two.yaml" --binary "$BIN" | tee "$STATE/apply5.log"
  grep -q "is up to date: nothing changed" "$STATE/apply5.log" || fail "apply after the prune was not a no-op"

  log "delete"
  lk cluster delete -f "$STATE/cluster-two.yaml" >/dev/null 2>&1 && fail "delete went ahead without --yes"
  lk cluster delete -f "$STATE/cluster-two.yaml" --yes --purge | tee "$STATE/delete.log"
  for i in 0 1; do
    r "${NAMES[$i]}" 'test ! -e /etc/systemd/system/litek8s-server.service && test ! -e /etc/systemd/system/litek8s-agent.service && test ! -e /var/lib/litek8s && ! pgrep -x litek8s' ||
      fail "${NAMES[$i]} still has litek8s"
    r "${NAMES[$i]}" '! sudo iptables-save | grep -q LITEK8S && ! ip link show litek8s0 2>/dev/null' || fail "${NAMES[$i]} still has the pod network"
  done
  grep -q "name: e2e" "$KUBECONFIG" && fail "the kubeconfig context is still there"

  log "PASS"
  [[ "${E2E_KEEP:-}" == 1 ]] || cmd_down
}

cmd_down() {
  stop_pid tunnel
  stop_pid http
  local n
  for n in "${NAMES[@]}"; do "$VM" down "$n" || true; done
}

case "${1:-run}" in
  run) cmd_run ;;
  down) cmd_down ;;
  *) echo "usage: $0 [run|down]" >&2; exit 2 ;;
esac
