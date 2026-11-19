#!/usr/bin/env bash
# End to end check of untrusted namespaces (src/kube/podsecurity.rs,
# src/kube/netpol.rs, src/kube/agent/netpol.rs): two fresh
# Ubuntu 24.04 VMs (bench/vm.sh), a server and an agent, made to look like a
# cloud cluster behind a VPN:
#
# - every node has a "public" address (203.0.113.x) and a "tailnet" address
#   (100.64.0.x) besides its LAN one;
# - a `ts-forward` chain first in the filter table's FORWARD accepts what
#   goes to the tailnet, as Tailscale's does;
# - the server answers on 169.254.169.254 like a cloud metadata service
#   whose user-data holds a secret.
#
# Each attack is run twice from a pod nobody configured (an image and a
# command, nothing else): in an ordinary namespace, where it works, and in
# a namespace labelled `litek8s.dev/isolation: untrusted`, where it must
# not: a privileged pod, more processes than the limit and a fork bomb, the
# kubelet of both nodes by every address, the API server, the metadata
# service, a pod of another namespace, a user namespace made with clone(2).
# And one that no NetworkPolicy stopped: the node's sshd over the bridge's
# IPv6 link-local address, closed for every pod.
#
#   bench/untrusted-e2e.sh           run it (VMs stay up on failure)
#   bench/untrusted-e2e.sh attacks   the attacks only, on the cluster of a kept run
#   bench/untrusted-e2e.sh startup [label] [namespace]   pod startup times only (same)
#   bench/untrusted-e2e.sh down      stop the VMs and the tunnel
#
# Env: LITEK8S_BIN (a linux binary; default: bench/build-linux.sh).
# LITEK8S_BASE_BIN: an older linux binary (0.2.26 or before). The cluster is
# then installed with it first, to show that its NetworkPolicy lets a pod
# out through the VPN, and upgraded in place: the policy's chains have to
# move from the filter table to mangle without the pod ever being open.
# Pod startup is measured on both. E2E_KEEP=1 keeps the VMs after a pass.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM="$HERE/vm.sh"
STATE="$WORK/untrusted-e2e"
KPORT="${E2E_KUBE_PORT:-16481}"
PPORT="${E2E_PROXY_PORT:-16482}"
export BENCH_LAN_PORT="${BENCH_LAN_PORT:-47750}"
SERVER=lku1 AGENT=lku2
SERVER_IP=192.168.77.51 AGENT_IP=192.168.77.52
SERVER_PUB=203.0.113.51 AGENT_PUB=203.0.113.52
SERVER_VPN=100.64.0.51 AGENT_VPN=100.64.0.52
IMAGE=curlimages/curl:8.10.1   # runs as a non-root user; has sh, curl, nslookup
KCFG="$STATE/kubeconfig"
STARTUP_RUNS="${E2E_STARTUP_RUNS:-30}"
mkdir -p "$STATE"

log() { echo "[untrusted-e2e] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --kubeconfig "$KCFG" --request-timeout=20s "$@"; }
r() { "$VM" ssh "$@"; }
arch() { [[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64; }
stop_pid() { local f="$STATE/$1.pid"; [[ -f "$f" ]] && { kill "$(cat "$f")" 2>/dev/null || true; rm -f "$f"; }; return 0; }

wait_for() { # seconds description command...
  local secs="$1" what="$2"; shift 2
  local deadline=$((SECONDS + secs))
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out after ${secs}s waiting for $what"
    sleep 1
  done
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
  r "$SERVER" 'sudo cat /etc/litek8s/kubeconfig.yaml' | sed "s#://127.0.0.1:6443#://127.0.0.1:$KPORT#" >"$KCFG"
}

# The addresses and the firewall of a cloud node behind a VPN.
dress() { # vm lan pub vpn other-lan other-pub other-vpn
  r "$1" "sudo sh -s" <<EOF
set -e
ip link show lkvpn0 >/dev/null 2>&1 || ip link add lkvpn0 type dummy
ip link set lkvpn0 up
ip addr replace $3/32 dev lkvpn0
ip addr replace $4/32 dev lkvpn0
ip route replace $6/32 via $5
ip route replace $7/32 via $5
iptables -N ts-forward 2>/dev/null || iptables -F ts-forward
iptables -A ts-forward -d 100.64.0.0/10 -j ACCEPT
EOF
}

# Tailscale puts its chain first every time it starts: after litek8s's.
ts_first() {
  for vm in "$SERVER" "$AGENT"; do
    r "$vm" 'sudo sh -c "iptables -D FORWARD -j ts-forward 2>/dev/null; iptables -I FORWARD 1 -j ts-forward"'
  done
}

metadata() {
  r "$SERVER" "sudo sh -s" <<'EOF'
set -e
ip addr replace 169.254.169.254/32 dev lkvpn0
mkdir -p /srv/lkmeta && echo 'tskey-auth-NOT-A-REAL-KEY' >/srv/lkmeta/user-data
systemctl stop lkmeta 2>/dev/null || true
systemd-run --quiet --unit lkmeta python3 -m http.server 80 --bind 169.254.169.254 --directory /srv/lkmeta
EOF
  r "$AGENT" "sudo ip route replace 169.254.169.254/32 via $SERVER_IP"
}

install_bin() { # binary: on both machines, the agent joined
  local bin="$1" out join
  for vm in "$SERVER" "$AGENT"; do
    "$VM" scp "$vm" "$bin" /tmp/litek8s.new
    r "$vm" 'sudo install -m 755 /tmp/litek8s.new /tmp/litek8s-install'
  done
  out="$(r "$SERVER" "sudo /tmp/litek8s-install install server --node-ip $SERVER_IP" 2>&1)" || { echo "$out" >&2; fail "server install"; }
  grep -q "litek8s server is ready" <<<"$out" || { echo "$out" >&2; fail "server install did not finish"; }
  join="$(grep -o 'install agent --join [^ ]* --token [^ ]*' <<<"$out" | head -1)"
  [[ -n "$join" ]] || fail "the server printed no join command"
  r "$AGENT" "sudo /tmp/litek8s-install $join --node-ip $AGENT_IP" >"$STATE/agent-install.log" 2>&1 || { cat "$STATE/agent-install.log" >&2; fail "agent install"; }
  ts_first
}

nodes_ready() {
  [[ "$(k get nodes -o jsonpath='{range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' | grep -c True)" == 2 ]]
}

# A pod as a third party writes it: an image and a command.
bare_pod() { # namespace name [node]
  k apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $2, namespace: $1, labels: {app: $2}}
spec:
  nodeName: ${3:-$AGENT}
  terminationGracePeriodSeconds: 1
  containers:
  - name: c
    image: $IMAGE
    command: [sleep, "3600"]
EOF
}

running() { [[ "$(k -n "$1" get pod "$2" -o jsonpath='{.status.phase}')" == Running ]]; }

# What a pod reaches, one "REACHED|blocked <name>" per line.
reach() { # namespace pod
  local bridge victim
  bridge="$(k get node "$AGENT" -o jsonpath='{.spec.podCIDR}' | sed 's#\.[0-9]*/.*#.1#')"
  victim="$(k -n victim get pod web -o jsonpath='{.status.podIP}')"
  k -n "$1" exec -i "$2" -- sh -s <<EOF
t() { if curl -sk -o /dev/null -m 4 "\$2" 2>/dev/null; then echo "REACHED \$1"; else echo "blocked \$1"; fi; }
t kubelet-own-bridge   https://$bridge:10250/pods
t kubelet-own-lan      https://$AGENT_IP:10250/pods
t kubelet-own-public   https://$AGENT_PUB:10250/pods
t kubelet-own-vpn      https://$AGENT_VPN:10250/pods
t kubelet-other-lan    https://$SERVER_IP:10250/pods
t kubelet-other-public https://$SERVER_PUB:10250/pods
t kubelet-other-vpn    https://$SERVER_VPN:10250/pods
t api-lan              https://$SERVER_IP:6443/version
t api-vpn              https://$SERVER_VPN:6443/version
t api-service          https://10.43.0.1/version
t metadata             http://169.254.169.254/user-data
t other-namespace-pod  http://$victim:8080/
t internet             https://example.com/
if nslookup kubernetes.default.svc.cluster.local >/dev/null 2>&1; then echo "REACHED dns"; else echo "blocked dns"; fi
EOF
}

expect_reach() { # title namespace pod reached-names...
  local title="$1" ns="$2" pod="$3"; shift 3
  local got want
  got="$(reach "$ns" "$pod")"
  echo "--- $title" >&2; echo "$got" | sed 's/^/    /' >&2
  want="$(printf '%s\n' "$@" | sort)"
  [[ "$(awk '$1=="REACHED"{print $2}' <<<"$got" | sort)" == "$want" ]] || fail "$title: expected to reach exactly: $*"
}

# The node's sshd over the bridge's IPv6 link-local address (kept in the
# state directory: the fixed agent takes the address away).
ssh6() { # namespace pod
  local ll
  ll="$(r "$AGENT" "ip -6 -o addr show dev litek8s0 scope link" | awk '{print $4}' | cut -d/ -f1 | head -1)"
  if [[ -n "$ll" ]]; then echo "$ll" >"$STATE/bridge-ll"; else ll="$(cat "$STATE/bridge-ll" 2>/dev/null || true)"; fi
  [[ -n "$ll" ]] || { echo "blocked (the bridge never had an IPv6 address)"; return; }
  if k -n "$1" exec "$2" -- sh -c "echo | nc -w 3 '$ll%eth0' 22 2>/dev/null" 2>/dev/null | grep -q '^SSH-'; then echo "REACHED"; else echo "blocked"; fi
}

# A new user namespace through clone(2), which unshare(1) does not use.
USERNS='import ctypes, os, platform
libc = ctypes.CDLL(None, use_errno=True)
pid = libc.syscall(220 if platform.machine() == "aarch64" else 56, 0x10000000 | 17, 0, 0, 0, 0)
if pid == 0:
    os._exit(0)
print("userns CREATED" if pid > 0 else "userns refused errno=%d" % ctypes.get_errno())
print("apparmor", open("/proc/self/attr/current").read().strip())'

python_pod() { # namespace [pod securityContext fields]
  k -n "$1" apply -f - <<EOF >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: py}
spec:
  nodeName: $AGENT
  terminationGracePeriodSeconds: 1
  securityContext: {runAsUser: 1000${2:+, $2}}
  containers:
  - {name: c, image: "python:3.12-alpine", command: [sleep, "3600"]}
EOF
  wait_for 240 "the python pod in $1" running "$1" py
}

# The policy an operator writes by hand for run pods: DNS and the internet,
# no private range, no metadata, no tailnet. Without the nodes' public
# addresses, which is the part one forgets.
hand_policy() { # namespace
  k apply -f - <<EOF
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: run-pods, namespace: $1}
spec:
  podSelector: {}
  policyTypes: [Ingress, Egress]
  ingress: []
  egress:
  - to:
    - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: kube-system}}
      podSelector: {matchLabels: {k8s-app: kube-dns}}
    ports: [{port: 53, protocol: UDP}, {port: 53, protocol: TCP}]
  - to:
    - ipBlock:
        cidr: 0.0.0.0/0
        except: [10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10, 127.0.0.0/8]
EOF
}

ALL=(kubelet-own-bridge kubelet-own-lan kubelet-own-public kubelet-own-vpn kubelet-other-lan kubelet-other-public kubelet-other-vpn api-lan api-vpn api-service metadata other-namespace-pod internet dns)

# Pod startup: from the create request to the pod Running or Succeeded.
startup() { # label namespace [extra spec yaml on one line]
  local label="$1" ns="$2"
  # Not through k(): the function's subshell would be what $! names.
  kubectl --kubeconfig "$KCFG" proxy --port "$PPORT" >/dev/null 2>&1 &
  local proxy=$!
  wait_for 20 "kubectl proxy" curl -fsS "http://127.0.0.1:$PPORT/version"
  python3 - "$PPORT" "$ns" "$AGENT" "$IMAGE" "$STARTUP_RUNS" "$label" <<'EOF' | tee -a "$STATE/startup.txt"
import json, statistics, sys, time, urllib.request
port, ns, node, image, runs, label = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), sys.argv[6]
base = f"http://127.0.0.1:{port}/api/v1/namespaces/{ns}/pods"
def call(method, url, body=None):
    req = urllib.request.Request(url, method=method, data=json.dumps(body).encode() if body else None, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)
times = []
for i in range(runs + 3):
    name = f"startup-{i}"
    pod = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name}, "spec": {"nodeName": node, "restartPolicy": "Never", "terminationGracePeriodSeconds": 1,
           "containers": [{"name": "c", "image": image, "imagePullPolicy": "IfNotPresent", "command": ["true"]}]}}
    t0 = time.monotonic()
    call("POST", base, pod)
    while True:
        phase = call("GET", f"{base}/{name}").get("status", {}).get("phase")
        if phase in ("Running", "Succeeded"):
            break
        if phase == "Failed" or time.monotonic() - t0 > 60:
            sys.exit(f"{label}: pod {name} is {phase}")
        time.sleep(0.004)
    if i >= 3:  # the first ones warm the image and the caches
        times.append(time.monotonic() - t0)
    call("DELETE", f"{base}/{name}")
    time.sleep(0.3)
times.sort()
print(f"startup {label:<34} n={len(times)} median={statistics.median(times)*1000:.0f}ms p90={times[int(len(times)*0.9)-1]*1000:.0f}ms min={times[0]*1000:.0f}ms max={times[-1]*1000:.0f}ms")
EOF
  kill "$proxy" 2>/dev/null || true
  wait "$proxy" 2>/dev/null || true
}

prepare_namespaces() {
  k create namespace plain --dry-run=client -o yaml | k apply -f -
  k create namespace victim --dry-run=client -o yaml | k apply -f -
  k -n victim apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata: {name: web, labels: {app: web}}
spec:
  nodeName: $AGENT
  terminationGracePeriodSeconds: 1
  containers:
  - name: c
    image: busybox:1.36
    command: [sh, -c, "mkdir /w && cd /w && echo secret >index.html && httpd -f -p 8080"]
EOF
  wait_for 180 "the victim pod" running victim web
}

cmd_startup() {
  : >"$STATE/startup.txt"
  startup "${1:-plain namespace}" "${2:-plain}"
}

# With the older binary: the hand-written policy does not hold.
phase_base() {
  log "installing the older litek8s ($("$LITEK8S_BASE_BIN" --version 2>/dev/null || echo "$LITEK8S_BASE_BIN"))"
  install_bin "$LITEK8S_BASE_BIN"
  tunnel
  wait_for 60 "the API through the tunnel" k get --raw /readyz
  wait_for 120 "both nodes Ready" nodes_ready
  prepare_namespaces
  k create namespace policed --dry-run=client -o yaml | k apply -f -
  hand_policy policed
  bare_pod policed turn
  wait_for 180 "the policed pod" running policed turn
  # What the policy forgot (the nodes' public addresses) and what goes
  # around it (the tailnet, accepted by ts-forward before the policy).
  expect_reach "older litek8s, hand-written NetworkPolicy" policed turn kubelet-own-public kubelet-other-public kubelet-other-vpn api-vpn internet dns
  local ssh
  ssh="$(ssh6 policed turn)"
  log "  the node's sshd over IPv6 link-local, around the policy: $ssh"
  [[ "$ssh" == REACHED ]] || fail "the older agent did not leave the IPv6 path open: nothing to compare with"
  r "$AGENT" 'sudo iptables-save -t filter' | grep -q -- '-A LITEK8S-FORWARD -j LITEK8S-NETPOL' || fail "the older agent's policy chain is not in the filter table"
  startup "older, plain namespace" plain
  startup "older, hand-written policy" policed
}

phase_upgrade() {
  log "upgrading both machines in place"
  for vm in "$SERVER" "$AGENT"; do
    "$VM" scp "$vm" "$BIN" /tmp/litek8s.new
    r "$vm" 'sudo install -m 755 /tmp/litek8s.new /tmp/litek8s-install'
  done
  r "$SERVER" "sudo /tmp/litek8s-install install server --node-ip $SERVER_IP" >/dev/null
  r "$AGENT" "sudo /tmp/litek8s-install install agent --join https://$SERVER_IP:6443 --node-ip $AGENT_IP" >/dev/null
  ts_first
  wait_for 60 "the API after the upgrade" k get --raw /readyz
  wait_for 120 "both nodes Ready" nodes_ready
  wait_for 60 "the policy chains in the mangle table" sh -c "'$VM' ssh $AGENT 'sudo iptables-save -t mangle' | grep -q -- '-A LITEK8S-NETPOL -s '"
  wait_for 60 "the older chains gone from the filter table" sh -c "! '$VM' ssh $AGENT 'sudo iptables-save -t filter' | grep -q LITEK8S-NETPOL"
  running policed turn || fail "the policed pod did not survive the upgrade"
  # The same policy, now out of reach of ts-forward. The public addresses
  # are still what the policy says: an ordinary namespace.
  expect_reach "new litek8s, same hand-written NetworkPolicy" policed turn kubelet-own-public kubelet-other-public internet dns
  # A pod started by the older agent still has IPv6 on its interface: the
  # node no longer answers there.
  local ssh
  ssh="$(ssh6 policed turn)"
  log "  the node's sshd over IPv6 link-local: $ssh"
  [[ "$ssh" == blocked* ]] || fail "the node still answers IPv6 on the bridge"
  startup "new, hand-written policy" policed
}

phase_new() {
  log "an ordinary namespace: every attack works"
  k apply -f - <<EOF >/dev/null 2>&1 && log "  privileged pod with the host's network and PIDs: admitted" || fail "the ordinary namespace refused a privileged pod"
apiVersion: v1
kind: Pod
metadata: {name: root, namespace: plain}
spec:
  nodeName: $AGENT
  hostNetwork: true
  hostPID: true
  terminationGracePeriodSeconds: 1
  containers:
  - {name: c, image: busybox:1.36, command: [sleep, "3600"], securityContext: {privileged: true}}
EOF
  k -n plain delete pod root --wait=false >/dev/null
  bare_pod plain turn
  wait_for 180 "the plain pod" running plain turn
  expect_reach "ordinary namespace, no policy" plain turn "${ALL[@]}"
  local procs
  procs="$(k -n plain exec turn -- sh -c 'i=0; while [ $i -lt 1000 ]; do sleep 120 & i=$((i+1)); done 2>/dev/null; ls -d /proc/[0-9]* | wc -l; cat /sys/fs/cgroup/pids.max')"
  log "  1000 processes asked for: $(head -1 <<<"$procs") running, pids.max=$(tail -1 <<<"$procs")"
  [[ "$(head -1 <<<"$procs")" -ge 1000 && "$(tail -1 <<<"$procs")" == max ]] || fail "the ordinary pod was limited: $procs"
  k -n plain delete pod turn --wait=true >/dev/null
  python_pod plain
  out="$(k -n plain exec py -- python3 -c "$USERNS")"
  log "  a user namespace through clone: $(tr '\n' ';' <<<"$out")"
  grep -q 'userns CREATED' <<<"$out" || fail "the ordinary pod could not make a user namespace: $out"
  k -n plain delete pod py --wait=true >/dev/null
  # Any namespace can ask for the AppArmor profile.
  python_pod plain 'appArmorProfile: {type: RuntimeDefault}'
  out="$(k -n plain exec py -- python3 -c "$USERNS")"
  log "  under appArmorProfile RuntimeDefault: $(tail -1 <<<"$out")"
  grep -q 'litek8s-default' <<<"$out" || fail "appArmorProfile RuntimeDefault did not apply the profile: $out"
  k -n plain delete pod py --wait=false >/dev/null
  startup "new, plain namespace" plain

  log "the same in a namespace labelled litek8s.dev/isolation=untrusted"
  k apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata: {name: runs, labels: {litek8s.dev/isolation: untrusted}}
EOF
  local out
  out="$(k apply -f - 2>&1 <<EOF || true
apiVersion: v1
kind: Pod
metadata: {name: root, namespace: runs}
spec:
  hostNetwork: true
  hostPID: true
  containers:
  - {name: c, image: busybox:1.36, command: [sleep, "3600"], securityContext: {privileged: true}}
EOF
)"
  grep -q 'violates PodSecurity "restricted:latest"' <<<"$out" || fail "the privileged pod was not refused: $out"
  log "  privileged pod: refused ($(grep -o 'violates PodSecurity.*' <<<"$out" | cut -c1-110)...)"
  out="$(k apply -f - 2>&1 <<EOF || true
apiVersion: v1
kind: Pod
metadata: {name: disk, namespace: runs}
spec:
  containers:
  - {name: c, image: $IMAGE, command: [sleep, "3600"], volumeMounts: [{name: host, mountPath: /host}]}
  volumes: [{name: host, hostPath: {path: /}}]
EOF
)"
  grep -q 'hostPath volumes' <<<"$out" || fail "the hostPath pod was not refused: $out"
  log "  hostPath pod: refused"

  # An image that runs as root does not start, whoever wrote the pod.
  k -n runs apply -f - <<EOF >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: rootimage}
spec:
  nodeName: $AGENT
  containers:
  - {name: c, image: busybox:1.36, command: [sleep, "3600"]}
EOF
  wait_for 120 "the root image to be refused" sh -c "kubectl --kubeconfig '$KCFG' -n runs get pod rootimage -o json | grep -q 'runAsNonRoot and image will run as root'"
  running runs rootimage && fail "an image that runs as root started in the untrusted namespace"
  log "  image running as root: not started"
  k -n runs delete pod rootimage --wait=false >/dev/null

  bare_pod runs turn
  wait_for 180 "the untrusted pod" running runs turn
  out="$(k -n runs exec turn -- sh -c 'id -u; grep -E "^(CapEff|NoNewPrivs|Seccomp):" /proc/self/status | tr -s "\t " " " | tr "\n" ";"; echo; ls /var/run/secrets/kubernetes.io/serviceaccount 2>/dev/null | wc -l; cat /sys/fs/cgroup/pids.max; cat /proc/sys/net/ipv6/conf/eth0/disable_ipv6 2>/dev/null || echo 1; cat /proc/sys/net/ipv6/conf/lo/disable_ipv6 2>/dev/null || echo 0; cat /proc/self/attr/current')"
  log "  the bare pod runs as: $(tr '\n' ' ' <<<"$out")"
  [[ "$(sed -n 1p <<<"$out")" != 0 ]] || fail "the untrusted pod runs as root"
  grep -q 'CapEff: 0000000000000000;NoNewPrivs: 1;Seccomp: 2;' <<<"$out" || fail "capabilities, no_new_privs or seccomp are not what restricted asks: $out"
  [[ "$(sed -n 3p <<<"$out")" == 0 ]] || fail "the untrusted pod has a ServiceAccount token"
  [[ "$(sed -n 4p <<<"$out")" == 256 ]] || fail "pids.max is not 256"
  [[ "$(sed -n 5p <<<"$out")" == 1 ]] || fail "IPv6 is on in the untrusted pod's interface"
  [[ "$(sed -n 6p <<<"$out")" == 0 ]] || fail "the untrusted pod lost ::1"
  # crun has a profile of its own on Ubuntu: the container's is stacked on it.
  [[ "$(sed -n 7p <<<"$out")" == *"litek8s-default ("* ]] || fail "the untrusted pod is not under the AppArmor profile: $(sed -n 7p <<<"$out")"
  [[ "$(ssh6 runs turn)" == blocked* ]] || fail "the untrusted pod reaches the node's sshd over IPv6"
  expect_reach "untrusted namespace, no policy at all" runs turn internet dns

  python_pod runs
  out="$(k -n runs exec py -- python3 -c "$USERNS")"
  log "  a user namespace through clone: $(tr '\n' ';' <<<"$out")"
  grep -q 'userns refused errno=1' <<<"$out" || fail "the untrusted pod made a user namespace: $out"
  k -n runs exec py -- python3 -c 'import threading; t = threading.Thread(target=print, args=("threads ok",)); t.start(); t.join()' | grep -q 'threads ok' || fail "threads do not start under the strict filter"
  k -n runs delete pod py --wait=false >/dev/null

  # At the limit the pod cannot even run `ls`: the node counts.
  k -n runs exec turn -- sh -c 'i=0; while [ $i -lt 1000 ]; do sleep 120 & i=$((i+1)); done' >/dev/null 2>&1 || true
  procs="$(r "$AGENT" "cat /sys/fs/cgroup/litek8s/pod$(k -n runs get pod turn -o jsonpath='{.metadata.uid}' | tr - _)/*/pids.peak | sort -n | tail -1")"
  log "  1000 processes asked for: at most ${procs:-?} ever ran"
  [[ -n "$procs" && "$procs" -le 256 ]] || fail "the untrusted pod had $procs processes"
  k -n runs delete pod turn --wait=true >/dev/null

  log "  a fork bomb for 20 s"
  k -n runs apply -f - <<EOF >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: bomb, labels: {app: bomb}}
spec:
  nodeName: $AGENT
  terminationGracePeriodSeconds: 1
  containers:
  - {name: c, image: $IMAGE, command: [sh, -c, "b() { b | b & }; b; sleep 3600"]}
EOF
  wait_for 120 "the bomb" running runs bomb
  sleep 20
  local uid peak
  uid="$(k -n runs get pod bomb -o jsonpath='{.metadata.uid}' | tr - _)"
  peak="$(r "$AGENT" "cat /sys/fs/cgroup/litek8s/pod$uid/*/pids.peak 2>/dev/null | sort -n | tail -1")"
  log "  the bomb's cgroup peaked at ${peak:-?} processes; the node: $(r "$AGENT" 'cat /proc/loadavg')"
  [[ -n "$peak" && "$peak" -le 256 ]] || fail "the fork bomb got to $peak processes"
  nodes_ready || fail "a node is not Ready after the fork bomb"
  # The node still starts pods while the bomb runs.
  bare_pod plain after-bomb
  wait_for 60 "a pod next to the bomb" running plain after-bomb
  k -n runs delete pod bomb --wait=true >/dev/null
  k -n plain delete pod after-bomb --wait=false >/dev/null

  # A policy opens what it names, never the guard.
  log "a NetworkPolicy in the untrusted namespace opens one pod, not the nodes"
  k -n runs apply -f - <<EOF >/dev/null
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: open}
spec:
  podSelector: {}
  policyTypes: [Egress]
  egress:
  - to: [{ipBlock: {cidr: 0.0.0.0/0}}]
  - to: [{namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: victim}}}]
    ports: [{port: 8080}]
  - to: [{namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: kube-system}}, podSelector: {matchLabels: {k8s-app: kube-dns}}}]
    ports: [{port: 53, protocol: UDP}, {port: 53, protocol: TCP}]
EOF
  bare_pod runs turn
  wait_for 180 "the untrusted pod" running runs turn
  sleep 1
  expect_reach "untrusted namespace, a policy allowing 0.0.0.0/0 and the victim namespace" runs turn other-namespace-pod internet dns
  k -n runs delete networkpolicy open >/dev/null
  k -n runs delete pod turn --wait=true >/dev/null
  startup "new, untrusted namespace" runs
}

cmd_run() {
  command -v kubectl >/dev/null || fail "needs kubectl"
  BIN="${LITEK8S_BIN:-$("$HERE/build-linux.sh" "$(arch)")}"
  : >"$STATE/startup.txt"
  log "booting $SERVER and $AGENT (fresh Ubuntu 24.04)"
  "$VM" up "$SERVER" 2461 2048 2 "$SERVER_IP"
  "$VM" up "$AGENT" 2462 2048 2 "$AGENT_IP"
  for vm in "$SERVER" "$AGENT"; do
    r "$vm" 'sudo sh -c "apt-get update -qq >/dev/null 2>&1; DEBIAN_FRONTEND=noninteractive apt-get install -y -qq crun iptables >/dev/null 2>&1; command -v crun iptables-restore >/dev/null"' || fail "crun/iptables on $vm"
  done
  dress "$SERVER" "$SERVER_IP" "$SERVER_PUB" "$SERVER_VPN" "$AGENT_IP" "$AGENT_PUB" "$AGENT_VPN"
  dress "$AGENT" "$AGENT_IP" "$AGENT_PUB" "$AGENT_VPN" "$SERVER_IP" "$SERVER_PUB" "$SERVER_VPN"
  metadata
  if [[ -n "${LITEK8S_BASE_BIN:-}" ]]; then
    phase_base
    phase_upgrade
  else
    install_bin "$BIN"
    tunnel
    wait_for 60 "the API through the tunnel" k get --raw /readyz
    wait_for 120 "both nodes Ready" nodes_ready
    prepare_namespaces
  fi
  phase_new
  log "pod startup (create request to Running/Succeeded, $STARTUP_RUNS pods each):"
  sed 's/^/    /' "$STATE/startup.txt" >&2
  log "PASS"
  [[ -n "${E2E_KEEP:-}" ]] || cmd_down
}

cmd_down() {
  stop_pid tunnel
  "$VM" down "$SERVER" 2>/dev/null || true
  "$VM" down "$AGENT" 2>/dev/null || true
}

case "${1:-run}" in
  run) cmd_run ;;
  startup) shift; cmd_startup "$@" ;;
  attacks) BIN="${LITEK8S_BIN:-}"; phase_new; log "PASS" ;;
  down) cmd_down ;;
  *) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
