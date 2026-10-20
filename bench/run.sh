#!/usr/bin/env bash
# Reproducible litek8s vs k3s footprint benchmark on a small Linux node.
#
#   bench/run.sh [phase...]      phases: litek8s rpc k3s k3s-minimal kube kube-crun
#                                        argocd-k3s argocd-litek8s obs-k3s obs-litek8s
#                                        (default: litek8s rpc k3s k3s-minimal)
#
# Each system gets a fresh throwaway VM from bench/vm.sh (Ubuntu 24.04 arm64,
# 2 vCPU, 1 GiB RAM, no swap: an AWS t4g.micro shape). The `rpc` phase reuses
# the litek8s VM as control plane and boots a second VM as an SSH-driven node.
# Raw JSON lands in bench/results/<timestamp>/; bench/report.py renders tables.
#
# Knobs (env): IDLE_SECS=300 SETTLE_SECS=60 TTR_RUNS=5 RPC_RUNS=50 COUNT_SECS=60
#              K3S_VERSION=<tag, default: k3s stable channel> VM_MEM=1024 VM_CPUS=2
#              KEEP_VMS=1 (leave VMs running) LITEK8S_BIN=<prebuilt linux binary>
#              LITEK8S_ARCH=amd64|arm64 (default: the host's; on x86_64 Linux the VMs are
#              amd64 under KVM, see vm.sh) NODE_ADDR/NODE_SSH_PORT (rpc on real hosts)
#              LITEK8S_COMMIT=<id> (recorded when the checkout has no .git)
#              argocd phases: ARGO_VM_MEM=2048 ARGO_VM_CPUS=2 ARGO_RUNS=3 ARGO_CHART_VERSION=10.10.5
#              obs phases: OBS_VM_MEM=3072 OBS_VM_CPUS=2 OBS_WARMUP_SECS=900 OBS_WINDOW_SECS=3600
#              OBS_QUERY_RUNS=20 KPS_CHART_VERSION LOKI_CHART_VERSION ALLOY_CHART_VERSION (default: latest)
# Real hosts instead of local VMs: see the BENCH_HOST_<name> note in bench/vm.sh
# (names: lk, lknode, k3s, k3smin).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
VM="$HERE/vm.sh"
IDLE_SECS="${IDLE_SECS:-300}"
SETTLE_SECS="${SETTLE_SECS:-60}"
TTR_RUNS="${TTR_RUNS:-5}"
RPC_RUNS="${RPC_RUNS:-50}"
COUNT_SECS="${COUNT_SECS:-60}"
K3S_VERSION="${K3S_VERSION:-}"
VM_MEM="${VM_MEM:-1024}"
VM_CPUS="${VM_CPUS:-2}"
OUT="${OUT:-$HERE/results/$(date -u +%Y%m%dT%H%M%SZ)}"
IMAGE="nginx:alpine"
LK_SOCK=/run/litek8s/daemon.sock
PORT_LK=2291 PORT_K3S=2292 PORT_K3SMIN=2293 PORT_NODE=2294 PORT_KUBE=2295 PORT_KUBECRUN=2296
PORT_ARGOK3S=2297 PORT_ARGOLK=2298 PORT_OBSK3S=2299 PORT_OBSLK=2300
GUEST_ARCH="${LITEK8S_ARCH:-$([[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64)}"
ARGO_VM_MEM="${ARGO_VM_MEM:-2048}"
ARGO_VM_CPUS="${ARGO_VM_CPUS:-2}"
ARGO_RUNS="${ARGO_RUNS:-3}"
ARGO_CHART_VERSION="${ARGO_CHART_VERSION:-10.10.5}"
OBS_VM_MEM="${OBS_VM_MEM:-3072}"
OBS_VM_CPUS="${OBS_VM_CPUS:-2}"
OBS_WARMUP_SECS="${OBS_WARMUP_SECS:-900}"
OBS_WINDOW_SECS="${OBS_WINDOW_SECS:-3600}"
OBS_QUERY_RUNS="${OBS_QUERY_RUNS:-20}"
HELM_VERSION="${HELM_VERSION:-v3.19.0}"
KUBECTL_VERSION="${KUBECTL_VERSION:-v1.34.1}"
KUBE_URL=http://127.0.0.1:6443

PHASES=("$@")
[[ ${#PHASES[@]} -gt 0 ]] || PHASES=(litek8s rpc k3s k3s-minimal)
mkdir -p "$OUT"
log() { printf '\033[1;34m[bench %s]\033[0m %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
rsh() { local vm="$1"; shift; "$VM" ssh "$vm" "$@"; }
rroot() { local vm="$1"; shift; "$VM" ssh "$vm" sudo bash -c "$(printf '%q' "$*")"; }
save() { cat >"$OUT/$1"; }

# ------------------------------------------------------------------ manifests

lk_manifest() { # name [nodeSelector-value]
  cat <<EOF
apiVersion: litek8s.dev/v1alpha1
kind: Deployment
metadata:
  name: $1
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
$( [[ -n "${2:-}" ]] && printf '      nodeSelector:\n        bench: %s\n' "$2" )
      containers:
        - name: nginx
          image: $IMAGE
          resources:
            limits:
              cpu: "500m"
              memory: "128Mi"
          ports:
            - containerPort: 80
          readinessProbe:
            tcpSocket:
              port: 80
            initialDelaySeconds: 0
            timeoutSeconds: 1
EOF
}

# Same container, limits and TCP probe. periodSeconds=2 mirrors litek8s's 2s
# reconcile tick so neither side wins on probe cadence.
k8s_manifest() {
  cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $1
spec:
  replicas: 1
  selector:
    matchLabels: {app: $1}
  template:
    metadata:
      labels: {app: $1}
    spec:
      containers:
        - name: nginx
          image: $IMAGE
          # Same limits as litek8s. Explicit small requests: k8s otherwise sets
          # requests=limits and 5x500m would not fit 2 vCPU (litek8s has no
          # CPU admission, so this keeps the workload identical).
          resources:
            requests: {cpu: "50m", memory: "32Mi"}
            limits: {cpu: "500m", memory: "128Mi"}
          ports:
            - containerPort: 80
          readinessProbe:
            tcpSocket: {port: 80}
            initialDelaySeconds: 0
            periodSeconds: 2
            timeoutSeconds: 1
EOF
}

# The same Deployment as JSON, for the litek8s Kubernetes API (no kubectl on
# the VM: probe.py POSTs it).
kube_manifest() {
  python3 - "$1" <<'PY'
import json, sys
name = sys.argv[1]
print(json.dumps({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name},
  "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}},
    "template": {"metadata": {"labels": {"app": name}}, "spec": {"containers": [{
      "name": "nginx", "image": "nginx:alpine",
      "resources": {"requests": {"cpu": "50m", "memory": "32Mi"}, "limits": {"cpu": "500m", "memory": "128Mi"}},
      "ports": [{"containerPort": 80}],
      "readinessProbe": {"tcpSocket": {"port": 80}, "initialDelaySeconds": 0, "periodSeconds": 2, "timeoutSeconds": 1}}]}}}}))
PY
}

push_manifest() { # vm path  (stdin = content)
  local tmp; tmp="$(mktemp)"; cat >"$tmp"; "$VM" scp "$1" "$tmp" "$2"; rm -f "$tmp"
}

# ------------------------------------------------------------------ common

vm_prepare() { # vm port
  "$VM" down "$1" >/dev/null 2>&1 || true
  "$VM" up "$1" "$2" "$VM_MEM" "$VM_CPUS"
  "$VM" scp "$1" "$HERE/node/probe.py" /tmp/probe.py
  rroot "$1" 'install -m 755 /tmp/probe.py /usr/local/bin/probe.py'
  # Let boot-time jobs (cloud-init, snap seeding, apt timers) finish.
  rroot "$1" 'systemctl is-system-running --wait >/dev/null 2>&1 || true; sleep 20'
}

# apt metadata is dropped first so the delta counts only what the system installed.
disk_used_kib() { rroot "$1" "apt-get clean; rm -rf /var/lib/apt/lists/*; sync; df -k --output=used / | tail -1 | tr -d ' '"; }
pkg_kib() { rsh "$1" "dpkg-query -Wf '\${Installed-Size}\n' | awk '{s+=\$1} END {print s}'"; }

measure_idle() { # vm system tag
  log "$2: settling ${SETTLE_SECS}s, then ${IDLE_SECS}s idle window ($3)"
  sleep "$SETTLE_SECS"
  rroot "$1" "probe.py mem" | save "$2.$3.mem.json"
  rroot "$1" "probe.py cpu $IDLE_SECS" | save "$2.$3.cpu.json"
}

install_docker() {
  rroot "$1" 'export DEBIAN_FRONTEND=noninteractive
    for i in 1 2 3 4 5; do apt-get update -qq && break; sleep 10; done
    apt-get install -y -qq docker.io >/dev/null
    systemctl enable --now docker >/dev/null 2>&1
    usermod -aG docker "$SUDO_USER"'
}

linux_binary() {
  if [[ -n "${LITEK8S_BIN:-}" ]]; then echo "$LITEK8S_BIN"; return; fi
  log "building linux/arm64 litek8s release binary"
  "$HERE/build-linux.sh" "$GUEST_ARCH"
}

lk_wait_ready() { # vm names...
  local vm="$1"; shift
  rroot "$vm" "python3 - $LK_SOCK $* <<'PY'
import sys, time
sys.path.insert(0, '/usr/local/bin')
import probe
sock, names = sys.argv[1], sys.argv[2:]
deadline = time.time() + 300
while time.time() < deadline:
    if all(probe.lk_ready(sock, n) for n in names):
        sys.exit(0)
    time.sleep(0.5)
sys.exit('not ready: ' + ' '.join(names))
PY"
}

lk_delete() { # vm name
  rroot "$1" "python3 -c \"import sys; sys.path.insert(0,'/usr/local/bin'); import probe; probe.lk_request('$LK_SOCK', {'DeleteDeployment': {'name': '$2'}})\""
}

# ------------------------------------------------------------------ litek8s

phase_litek8s() {
  local vm=lk bin
  bin="$(linux_binary)"
  log "litek8s: preparing VM"
  vm_prepare "$vm" "$PORT_LK"
  rroot "$vm" "probe.py mem" | save litek8s.baseline.mem.json
  local disk0 pkg0; disk0="$(disk_used_kib "$vm")"; pkg0="$(pkg_kib "$vm")"

  log "litek8s: installing docker.io + litek8s"
  install_docker "$vm"
  "$VM" scp "$vm" "$bin" /tmp/litek8s
  rroot "$vm" 'install -m 755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s
    mkdir -p /run/litek8s /var/lib/litek8s
    systemd-run --unit=litek8s --quiet /usr/local/bin/litek8s --socket-path '"$LK_SOCK"' \
      serve --state-file /var/lib/litek8s/state.json --peer-addr 127.0.0.1:4001
    for i in $(seq 1 100); do [ -S '"$LK_SOCK"' ] && break; sleep 0.1; done'
  sleep 3
  local disk1 pkg1
  disk1="$(disk_used_kib "$vm")"; pkg1="$(pkg_kib "$vm")"
  rroot "$vm" "python3 - <<'PY'
import json, os, subprocess
du = lambda p: int(subprocess.run(['du','-sk',p],capture_output=True,text=True).stdout.split()[0]) if os.path.exists(p) else 0
ver = subprocess.run(['dpkg-query','-Wf','\${Version}','docker.io'],capture_output=True,text=True).stdout
print(json.dumps({
  'disk_used_delta_kib': $disk1 - $disk0,
  'pkg_installed_delta_kib': $pkg1 - $pkg0,
  'litek8s_binary_kib': os.path.getsize('/usr/local/bin/litek8s')//1024,
  'var_lib_docker_kib': du('/var/lib/docker'),
  'var_lib_containerd_kib': du('/var/lib/containerd'),
  'docker_io_version': ver,
}))
PY" | save litek8s.size.json

  measure_idle "$vm" litek8s idle0

  log "litek8s: time-to-ready x$TTR_RUNS (image pre-pulled)"
  rroot "$vm" "docker pull -q $IMAGE >/dev/null"
  lk_manifest ttr | push_manifest "$vm" /tmp/ttr.yaml
  {
    echo '['
    for i in $(seq 1 "$TTR_RUNS"); do
      rroot "$vm" "probe.py ttr-litek8s $LK_SOCK /tmp/ttr.yaml ttr"
      lk_delete "$vm" ttr
      rroot "$vm" 'for i in $(seq 1 100); do [ -z "$(docker ps -aq --filter label=litek8s.deployment=ttr)" ] && break; sleep 0.2; done; sleep 2'
      [[ $i -lt $TTR_RUNS ]] && echo ','
    done
    echo ']'
  } | save litek8s.ttr.json

  log "litek8s: 5 nginx deployments"
  for i in 0 1 2 3 4; do
    lk_manifest "web-$i" | push_manifest "$vm" "/tmp/web-$i.yaml"
    rroot "$vm" "litek8s --socket-path $LK_SOCK apply -f /tmp/web-$i.yaml >/dev/null"
  done
  lk_wait_ready "$vm" web-0 web-1 web-2 web-3 web-4
  measure_idle "$vm" litek8s idle5
  rroot "$vm" "stat -c %s /var/lib/litek8s/state.json" | save litek8s.idle5.statefile-bytes.txt
}

# ------------------------------------------------------------------ kube

# litek8s's Kubernetes path: `litek8s apiserver` with its controllers and the
# local node agent in one process. Same workload as the k3s phases.
#   phase_kube docker  -> pods on Docker (dockerd + containerd + shims)
#   phase_kube crun    -> pods on crun directly (no daemon; one small shim per container)
phase_kube() {
  local rt="${1:-docker}" tag vm bin port
  if [[ "$rt" == crun ]]; then tag=kubecrun vm=kubecrun port="$PORT_KUBECRUN"; else tag=kube vm=kube port="$PORT_KUBE"; fi
  bin="$(linux_binary)"
  log "$tag: preparing VM"
  vm_prepare "$vm" "$port"
  rroot "$vm" "probe.py mem" | save $tag.baseline.mem.json
  local disk0 pkg0; disk0="$(disk_used_kib "$vm")"; pkg0="$(pkg_kib "$vm")"

  if [[ "$rt" == crun ]]; then
    log "$tag: installing crun + litek8s apiserver"
    rroot "$vm" 'export DEBIAN_FRONTEND=noninteractive
      for i in 1 2 3 4 5; do apt-get update -qq && break; sleep 10; done
      apt-get install -y -qq crun iptables >/dev/null'
  else
    log "$tag: installing docker.io + litek8s apiserver"
    install_docker "$vm"
  fi
  "$VM" scp "$vm" "$bin" /tmp/litek8s
  rroot "$vm" 'install -m 755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s
    mkdir -p /var/lib/litek8s
    systemd-run --unit=litek8s --quiet /usr/local/bin/litek8s apiserver --listen 127.0.0.1:6443 \
      --data-dir /var/lib/litek8s/apiserver --kubeconfig /root/litek8s.kubeconfig --runtime '"$rt"'
    for i in $(seq 1 100); do curl -sf '"$KUBE_URL"'/readyz >/dev/null && break; sleep 0.1; done'
  if [[ "$rt" == docker ]]; then
    # Pull the pod sandbox and service-proxy helper images now, as k3s ships its pause image.
    rroot "$vm" 'docker pull -q busybox:1.36 >/dev/null; docker pull -q rancher/klipper-lb:v0.4.13 >/dev/null'
  fi
  sleep 3
  local disk1 pkg1
  disk1="$(disk_used_kib "$vm")"; pkg1="$(pkg_kib "$vm")"
  rroot "$vm" "python3 - <<'PY'
import json, os, subprocess
du = lambda p: int(subprocess.run(['du','-sk',p],capture_output=True,text=True).stdout.split()[0]) if os.path.exists(p) else 0
pkg = 'crun' if '$rt' == 'crun' else 'docker.io'
ver = subprocess.run(['dpkg-query','-Wf','\${Version}',pkg],capture_output=True,text=True).stdout
print(json.dumps({
  'disk_used_delta_kib': $disk1 - $disk0,
  'pkg_installed_delta_kib': $pkg1 - $pkg0,
  'litek8s_binary_kib': os.path.getsize('/usr/local/bin/litek8s')//1024,
  'var_lib_docker_kib': du('/var/lib/docker'),
  'runtime': '$rt',
  'runtime_version': ver,
}))
PY" | save $tag.size.json

  measure_idle "$vm" $tag idle0

  log "$tag: time-to-ready x$TTR_RUNS (image pre-pulled)"
  kube_manifest ttr | push_manifest "$vm" /tmp/ttr.json
  if [[ "$rt" == crun ]]; then
    # No image CLI without Docker: one untimed run pulls the image.
    rroot "$vm" "probe.py ttr-kube $KUBE_URL /tmp/ttr.json ttr >/dev/null; probe.py kube-delete $KUBE_URL ttr >/dev/null; sleep 2"
  else
    rroot "$vm" "docker pull -q $IMAGE >/dev/null"
  fi
  {
    echo '['
    for i in $(seq 1 "$TTR_RUNS"); do
      rroot "$vm" "probe.py ttr-kube $KUBE_URL /tmp/ttr.json ttr"
      rroot "$vm" "probe.py kube-delete $KUBE_URL ttr >/dev/null; sleep 2"
      [[ $i -lt $TTR_RUNS ]] && echo ','
    done
    echo ']'
  } | save $tag.ttr.json

  log "$tag: 5 nginx deployments"
  for i in 0 1 2 3 4; do
    kube_manifest "web-$i" | push_manifest "$vm" "/tmp/web-$i.json"
    rroot "$vm" "probe.py kube-apply $KUBE_URL /tmp/web-$i.json >/dev/null"
  done
  rroot "$vm" "probe.py kube-ready $KUBE_URL web-0 web-1 web-2 web-3 web-4 >/dev/null"
  measure_idle "$vm" $tag idle5
  # The object log, not the crun image store and container root filesystems
  # under node-agent/crun (du would also descend into the mounted rootfs).
  rroot "$vm" "du -sbx --exclude=crun /var/lib/litek8s/apiserver | cut -f1" | save $tag.idle5.statefile-bytes.txt
  [[ -n "${KEEP_VMS:-}" ]] || "$VM" down "$vm"
}

# ------------------------------------------------------------------ k3s

phase_k3s() { # tag port extra-install-args
  local tag="$1" port="$2" extra="$3" vm="$1"
  log "$tag: preparing VM"
  vm_prepare "$vm" "$port"
  rroot "$vm" "probe.py mem" | save "$tag.baseline.mem.json"
  local disk0; disk0="$(disk_used_kib "$vm")"

  log "$tag: installing k3s ${K3S_VERSION:-stable} $extra"
  rroot "$vm" "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' sh -s - $extra >/dev/null"
  rroot "$vm" 'export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
    for i in $(seq 1 120); do k3s kubectl get nodes 2>/dev/null | grep -q " Ready" && break; sleep 2; done
    # wait for the bundled helm-install jobs (traefik), then for every addon pod
    for i in $(seq 1 60); do k3s kubectl get pods -n kube-system --no-headers 2>/dev/null | grep -q . && break; sleep 2; done
    k3s kubectl wait --for=condition=complete job --all -n kube-system --timeout=300s >/dev/null 2>&1 || true
    sleep 5
    for i in $(seq 1 150); do
      pods=$(k3s kubectl get pods -n kube-system --no-headers 2>/dev/null | grep -v Completed | wc -l)
      ready=$(k3s kubectl get pods -n kube-system --no-headers 2>/dev/null | grep -v Completed | awk "{split(\$2,a,\"/\"); if (a[1]==a[2] && \$3==\"Running\") n++} END {print n+0}")
      [ "$pods" -gt 0 ] && [ "$pods" = "$ready" ] && break; sleep 2
    done
    k3s kubectl get pods -A --no-headers'  >"$OUT/$tag.system-pods.txt"
  local disk1; disk1="$(disk_used_kib "$vm")"
  rroot "$vm" "python3 - <<'PY'
import json, os, subprocess
du = lambda p: int(subprocess.run(['du','-sk',p],capture_output=True,text=True).stdout.split()[0]) if os.path.exists(p) else 0
ver = subprocess.run(['k3s','--version'],capture_output=True,text=True).stdout.splitlines()[0]
print(json.dumps({
  'disk_used_delta_kib': $disk1 - $disk0,
  'k3s_binary_kib': os.path.getsize('/usr/local/bin/k3s')//1024,
  'var_lib_rancher_k3s_kib': du('/var/lib/rancher/k3s'),
  'k3s_data_dir_kib': du('/var/lib/rancher/k3s/data'),
  'k3s_containerd_store_kib': du('/var/lib/rancher/k3s/agent/containerd'),
  'k3s_version': ver,
}))
PY" | save "$tag.size.json"

  measure_idle "$vm" "$tag" idle0

  log "$tag: time-to-ready x$TTR_RUNS (image pre-pulled)"
  rroot "$vm" "k3s ctr -n k8s.io images pull docker.io/library/$IMAGE >/dev/null"
  k8s_manifest ttr | push_manifest "$vm" /tmp/ttr.yaml
  {
    echo '['
    for i in $(seq 1 "$TTR_RUNS"); do
      rroot "$vm" "probe.py ttr-k3s /tmp/ttr.yaml ttr"
      rroot "$vm" 'k3s kubectl delete -f /tmp/ttr.yaml --wait=true >/dev/null
        k3s kubectl wait --for=delete pod -l app=ttr --timeout=120s >/dev/null 2>&1 || true; sleep 2'
      [[ $i -lt $TTR_RUNS ]] && echo ','
    done
    echo ']'
  } | save "$tag.ttr.json"

  log "$tag: 5 nginx deployments"
  for i in 0 1 2 3 4; do k8s_manifest "web-$i"; echo '---'; done | push_manifest "$vm" /tmp/web.yaml
  rroot "$vm" 'k3s kubectl apply -f /tmp/web.yaml >/dev/null
    k3s kubectl wait --for=condition=Available deploy --all --timeout=300s >/dev/null'
  measure_idle "$vm" "$tag" idle5
  rroot "$vm" "du -sb /var/lib/rancher/k3s/server/db | cut -f1" | save "$tag.idle5.statefile-bytes.txt"
  [[ -n "${KEEP_VMS:-}" ]] || "$VM" down "$vm"
}

# ------------------------------------------------------------------ rpc

phase_rpc() {
  local cp=lk node=lknode bin
  bin="$(linux_binary)"
  if ! rsh "$cp" "test -S $LK_SOCK" 2>/dev/null; then
    log "rpc: litek8s control-plane VM not running; running litek8s phase first"
    phase_litek8s
  fi
  log "rpc: preparing node VM"
  vm_prepare "$node" "$PORT_NODE"
  install_docker "$node"
  "$VM" scp "$node" "$bin" /tmp/litek8s
  # Counting shim: a no-op exec unless /tmp/litek8s-rpc.count exists, in which
  # case it logs the request line first. Lets one node registration serve both
  # the RPC census (flag on) and the CPU window (flag off, ~dash exec overhead).
  rroot "$node" "install -m 755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s
    cat >/usr/local/bin/litek8s-agent-shim <<'SH'
#!/bin/sh
[ -e /tmp/litek8s-rpc.count ] || exec /usr/local/bin/litek8s \"\$@\"
IFS= read -r line
echo \"\$(date +%s.%N) \$line\" | cut -c1-160 >> /tmp/litek8s-rpc.log
printf '%s\n' \"\$line\" | exec /usr/local/bin/litek8s \"\$@\"
SH
    chmod 755 /usr/local/bin/litek8s-agent-shim
    docker pull -q $IMAGE >/dev/null"

  # Control plane -> node SSH: a key minted on the control plane and authorized
  # on the node (no local private key leaves this machine). Local VMs reach each
  # other through the host's port forward; for real hosts set NODE_ADDR (the
  # node's private IP as seen from the control plane) and NODE_SSH_PORT.
  local node_addr="${NODE_ADDR:-10.0.2.2}" node_port="${NODE_SSH_PORT:-$PORT_NODE}" node_user pub
  node_user="$(rsh "$node" whoami)"
  pub="$(rroot "$cp" "install -d -m 700 /root/.ssh; [ -f /root/.ssh/bench_key ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/bench_key; cat /root/.ssh/bench_key.pub")"
  rsh "$node" "umask 077; mkdir -p ~/.ssh; grep -qxF '$pub' ~/.ssh/authorized_keys 2>/dev/null || echo '$pub' >> ~/.ssh/authorized_keys"
  rroot "$cp" "cat >/root/.ssh/config <<EOF
Host lknode
  HostName $node_addr
  Port $node_port
  User $node_user
  IdentityFile /root/.ssh/bench_key
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
EOF"
  for i in 0 1 2 3 4; do lk_delete "$cp" "web-$i" 2>/dev/null || true; done
  rroot "$cp" 'for i in $(seq 1 150); do [ -z "$(docker ps -aq --filter label=litek8s.managed=true)" ] && break; sleep 0.2; done'
  rroot "$cp" "litek8s --socket-path $LK_SOCK node add lknode --ssh lknode --remote-binary /usr/local/bin/litek8s-agent-shim --label bench=remote >/dev/null"

  log "rpc: per-RPC latency x$RPC_RUNS"
  local SSHO="ssh -o BatchMode=yes -o ConnectTimeout=5 -o ControlMaster=auto -o ControlPath=/tmp/bench-cm-%C -o ControlPersist=60s lknode"
  rroot "$cp" "$SSHO true"  # open the master
  rpc_case() { # tag payload remote-cmd
    rroot "$node" "probe.py cpu-snap /tmp/rpc0.json"
    rroot "$cp" "probe.py ssh-rpc $RPC_RUNS '$2' -- $SSHO $3" >"$OUT/rpc.$1.caller.json"
    rroot "$node" "probe.py cpu-diff /tmp/rpc0.json" >"$OUT/rpc.$1.node-cpu.json"
  }
  # Null case: the snapshot/diff tooling's own cost, subtracted by report.py.
  rroot "$node" "probe.py cpu-snap /tmp/rpc0.json"
  rroot "$node" "probe.py cpu-diff /tmp/rpc0.json" >"$OUT/rpc.null.node-cpu.json"
  rpc_case ssh-true '"Ping"' true
  rpc_case ping '"Ping"' "/usr/local/bin/litek8s agent"
  rpc_case list '{"ListContainersForDeployment":{"name":"web-0"}}' "/usr/local/bin/litek8s agent"
  # Same requests straight to the resident `agent --serve` socket: the floor a
  # push-based agent with a persistent connection would pay (no ssh, no exec).
  rsh "$node" "probe.py agent-direct /run/user/\$(id -u)/litek8s/agent.sock $RPC_RUNS '\"Ping\"'" >"$OUT/rpc.direct-ping.json"
  rsh "$node" "probe.py agent-direct /run/user/\$(id -u)/litek8s/agent.sock $RPC_RUNS '{\"ListContainersForDeployment\":{\"name\":\"web-0\"}}'" >"$OUT/rpc.direct-list.json"

  log "rpc: 5 deployments on the remote node"
  for i in 0 1 2 3 4; do
    lk_manifest "web-$i" remote | push_manifest "$cp" "/tmp/rweb-$i.yaml"
    rroot "$cp" "litek8s --socket-path $LK_SOCK apply -f /tmp/rweb-$i.yaml >/dev/null"
  done
  lk_wait_ready "$cp" web-0 web-1 web-2 web-3 web-4
  sleep 20
  log "rpc: RPC census for ${COUNT_SECS}s"
  rroot "$node" "rm -f /tmp/litek8s-rpc.log; touch /tmp/litek8s-rpc.count"
  sleep "$COUNT_SECS"
  rroot "$node" "rm -f /tmp/litek8s-rpc.count; sleep 2; cat /tmp/litek8s-rpc.log" >"$OUT/rpc.census.log"
  echo "$COUNT_SECS" >"$OUT/rpc.census.window"

  log "rpc: ${IDLE_SECS}s idle window on node + control plane"
  sleep 10
  rroot "$node" "probe.py mem" | save rpc.node.idle5.mem.json
  rroot "$cp" "probe.py mem" | save rpc.cp.idle5.mem.json
  rroot "$cp" "probe.py cpu $IDLE_SECS" >"$OUT/rpc.cp.idle5.cpu.json" &
  rroot "$node" "probe.py cpu $IDLE_SECS" >"$OUT/rpc.node.idle5.cpu.json"
  wait
  [[ -n "${KEEP_VMS:-}" ]] || { "$VM" down "$node"; "$VM" down "$cp"; }
}

# ------------------------------------------------------------------ argocd

# Argo CD (pinned Helm chart, chart defaults, non-HA) on k3s minimal and on
# litek8s apiserver with crun, on identical VMs (ARGO_VM_MEM / ARGO_VM_CPUS).
# The same kubectl, helm, chart and node/argocd.py driver run on both.
#   phase_argocd k3s | litek8s

argo_tools() { # -> dir with helm, kubectl and the chart for the guest arch
  local arch="$GUEST_ARCH" dir="$HERE/.work/argocd" helm=helm
  mkdir -p "$dir/$arch"
  [[ -x "$dir/$arch/helm" ]] || curl -fsSL "https://get.helm.sh/helm-$HELM_VERSION-linux-$arch.tar.gz" | tar xz -C "$dir/$arch" --strip-components 1 "linux-$arch/helm"
  [[ -x "$dir/$arch/kubectl" ]] || { curl -fsSL -o "$dir/$arch/kubectl" "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/linux/$arch/kubectl"; chmod +x "$dir/$arch/kubectl"; }
  if [[ ! -f "$dir/argo-cd-$ARGO_CHART_VERSION.tgz" ]]; then
    # Private helm dirs: other checkouts share the default cache.
    command -v helm >/dev/null || helm="$dir/$arch/helm"  # a Linux host of the guests' arch
    HELM_CACHE_HOME="$dir/helm/cache" HELM_CONFIG_HOME="$dir/helm/config" HELM_DATA_HOME="$dir/helm/data" \
      "$helm" pull argo-cd --repo https://argoproj.github.io/argo-helm --version "$ARGO_CHART_VERSION" -d "$dir" >&2
  fi
  echo "$dir"
}

# The machine running the VMs: load average and CPU busy counters (Linux
# only; on macOS an empty object). Saved around each phase and idle window,
# so a busy host shows up next to the numbers it may have skewed.
host_snap() {
  python3 - <<'PY'
import json, os, time
try:
    cpu = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    print(json.dumps({"t": time.time(), "loadavg": open("/proc/loadavg").read().split()[:3],
                      "busy_ticks": sum(cpu) - cpu[3] - cpu[4], "total_ticks": sum(cpu), "ncpu": os.cpu_count()}))
except OSError:
    print("{}")
PY
}

phase_argocd() {
  local sys="$1" vm port tag kc
  local VM_MEM="$ARGO_VM_MEM" VM_CPUS="$ARGO_VM_CPUS"
  if [[ "$sys" == k3s ]]; then vm=argok3s port="$PORT_ARGOK3S" kc=/etc/rancher/k3s/k3s.yaml
  else vm=argolk port="$PORT_ARGOLK" kc=/root/litek8s.kubeconfig; fi
  tag="argocd-$sys"
  local tools; tools="$(argo_tools)"
  host_snap | save "$tag.host.start.json"
  local bin=""; [[ "$sys" == litek8s ]] && bin="$(linux_binary)"
  log "$tag: preparing VM (${VM_MEM} MiB, ${VM_CPUS} vCPU)"
  vm_prepare "$vm" "$port"
  local a="$GUEST_ARCH"
  "$VM" scp "$vm" "$tools/$a/helm" /tmp/helm
  "$VM" scp "$vm" "$tools/$a/kubectl" /tmp/kubectl
  "$VM" scp "$vm" "$tools/argo-cd-$ARGO_CHART_VERSION.tgz" /tmp/argo-cd.tgz
  "$VM" scp "$vm" "$HERE/node/argocd.py" /tmp/argocd.py
  for f in values.yaml guestbook.yaml helm-guestbook.yaml; do "$VM" scp "$vm" "$HERE/argocd/$f" "/tmp/$f"; done
  rroot "$vm" 'install -m 755 /tmp/helm /tmp/kubectl /tmp/argocd.py /usr/local/bin/
    mkdir -p /root/argocd && mv /tmp/argo-cd.tgz /tmp/values.yaml /tmp/guestbook.yaml /tmp/helm-guestbook.yaml /root/argocd/'
  if [[ "$a" == arm64 ]]; then
    # The example apps' image (gcr.io/google-samples/gb-frontend:v5) is
    # amd64 only: run it emulated through binfmt, the same on both systems.
    rroot "$vm" 'export DEBIAN_FRONTEND=noninteractive
      for i in 1 2 3 4 5; do apt-get update -qq && break; sleep 10; done
      apt-get install -y -qq qemu-user-static >/dev/null 2>&1'
  fi

  if [[ "$sys" == k3s ]]; then
    log "$tag: installing k3s ${K3S_VERSION:-stable} (minimal)"
    rroot "$vm" "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' sh -s - --disable=traefik,servicelb,metrics-server,local-storage >/dev/null"
  else
    log "$tag: installing crun + litek8s apiserver"
    rroot "$vm" 'export DEBIAN_FRONTEND=noninteractive
      for i in 1 2 3 4 5; do apt-get update -qq && break; sleep 10; done
      apt-get install -y -qq crun iptables >/dev/null'
    "$VM" scp "$vm" "$bin" /tmp/litek8s
    rroot "$vm" 'install -m 755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s
      mkdir -p /var/lib/litek8s
      systemd-run --unit=litek8s --quiet /usr/local/bin/litek8s apiserver --listen 127.0.0.1:6443 \
        --data-dir /var/lib/litek8s/apiserver --kubeconfig /root/litek8s.kubeconfig --runtime crun
      for i in $(seq 1 100); do curl -sf '"$KUBE_URL"'/readyz >/dev/null && break; sleep 0.1; done'
  fi
  # Every later command runs as root with this kubeconfig.
  local K="export KUBECONFIG=$kc; cd /root/argocd;"
  rroot "$vm" "$K"'for i in $(seq 1 120); do kubectl get nodes 2>/dev/null | grep -q " Ready" && break; sleep 2; done
    for i in $(seq 1 150); do
      n=$(kubectl get pods -A --no-headers 2>/dev/null | grep -vc Completed); r=$(kubectl get pods -A --no-headers 2>/dev/null | awk "{split(\$3,a,\"/\"); if (a[1]==a[2] && \$4==\"Running\") c++} END {print c+0}")
      [ "$n" = "$r" ] && break; sleep 2
    done; kubectl get pods -A --no-headers' >"$OUT/$tag.system-pods.txt"

  log "$tag: system only: settle ${SETTLE_SECS}s, memory, 60s of API requests"
  sleep "$SETTLE_SECS"
  rroot "$vm" "probe.py mem" | save "$tag.idle0.mem.json"
  rroot "$vm" "$K argocd.py metrics > /tmp/m0.json; sleep 60; argocd.py metrics > /tmp/m1.json; python3 -c 'import json; print(json.dumps({\"before\": json.load(open(\"/tmp/m0.json\")), \"after\": json.load(open(\"/tmp/m1.json\"))}))'" | save "$tag.api-idle0.json"

  log "$tag: helm install (cold: images pulled)"
  rroot "$vm" "$K argocd.py install argo-cd.tgz values.yaml" | save "$tag.install-cold.json"
  {
    echo '['
    for i in $(seq 1 "$ARGO_RUNS"); do
      log "$tag: helm uninstall + install, warm images ($i/$ARGO_RUNS)"
      rroot "$vm" "$K argocd.py uninstall >/dev/null; sleep 10; argocd.py install argo-cd.tgz values.yaml"
      [[ $i -lt $ARGO_RUNS ]] && echo ','
    done
    echo ']'
  } | save "$tag.install-warm.json"

  log "$tag: Argo CD idle (no app): settle ${SETTLE_SECS}s, memory"
  sleep "$SETTLE_SECS"
  rroot "$vm" "probe.py mem" | save "$tag.argo-idle.mem.json"

  log "$tag: GitOps work (memory sampled every 5s)"
  rroot "$vm" "rm -f /tmp/mem-sync.jsonl; nohup probe.py mem-sample 5 /tmp/mem-sync.jsonl >/dev/null 2>&1 & echo \$! > /tmp/mem-sample.pid"
  rroot "$vm" "$K argocd.py metrics" | save "$tag.api-sync.before.json"
  local t0; t0=$(date +%s)
  for app in guestbook helm-guestbook; do
    {
      echo '['
      for i in $(seq 1 $((ARGO_RUNS + 1))); do
        # The first run clones the repository and pulls the app's image.
        [[ $i -gt 1 ]] && rroot "$vm" "$K argocd.py app-delete $app >/dev/null; sleep 5"
        rroot "$vm" "$K argocd.py app-create $app.yaml $app"
        [[ $i -le $ARGO_RUNS ]] && echo ','
      done
      echo ']'
    } | save "$tag.app-create.$app.json"
  done
  {
    echo '['
    for i in $(seq 1 $((2 * ARGO_RUNS))); do
      rroot "$vm" "$K sleep 5; argocd.py app-scale helm-guestbook $(( i % 2 == 1 ? 2 : 1 ))"
      [[ $i -lt $((2 * ARGO_RUNS)) ]] && echo ','
    done
    echo ']'
  } | save "$tag.app-scale.json"
  {
    echo '['
    for i in $(seq 1 "$ARGO_RUNS"); do
      rroot "$vm" "$K sleep 10; argocd.py drift guestbook guestbook guestbook-ui"
      [[ $i -lt $ARGO_RUNS ]] && echo ','
    done
    echo ']'
  } | save "$tag.drift.json"
  rroot "$vm" "$K argocd.py app-delete helm-guestbook >/dev/null"
  echo "{\"seconds\": $(( $(date +%s) - t0 ))}" | save "$tag.sync-window.json"
  rroot "$vm" "$K argocd.py metrics" | save "$tag.api-sync.after.json"
  rroot "$vm" 'kill $(cat /tmp/mem-sample.pid); cat /tmp/mem-sync.jsonl' | save "$tag.mem-sync.jsonl"

  log "$tag: one app synced: settle ${SETTLE_SECS}s, memory, ${IDLE_SECS}s CPU + API requests"
  sleep "$SETTLE_SECS"
  rroot "$vm" "probe.py mem" | save "$tag.idle-app.mem.json"
  rroot "$vm" "$K argocd.py metrics" | save "$tag.api-idle.before.json"
  host_snap | save "$tag.host.idle-start.json"
  rroot "$vm" "probe.py cpu $IDLE_SECS" | save "$tag.idle-app.cpu.json"
  host_snap | save "$tag.host.idle-end.json"
  rroot "$vm" "$K argocd.py metrics" | save "$tag.api-idle.after.json"
  rroot "$vm" "probe.py mem" | save "$tag.idle-app-end.mem.json"

  rroot "$vm" "$K"' python3 - <<'"'PY'"'
import json, subprocess
run = lambda *a: subprocess.run(a, capture_output=True, text=True).stdout.strip()
pods = json.loads(run("kubectl", "get", "pods", "-A", "-o", "json"))["items"]
print(json.dumps({
  "server_version": json.loads(run("kubectl", "version", "-o", "json")).get("serverVersion", {}).get("gitVersion"),
  "k3s_version": run("sh", "-c", "k3s --version 2>/dev/null | head -1"),
  "crun_version": run("sh", "-c", "crun --version 2>/dev/null | head -1"),
  "chart": run("sh", "-c", "helm list -n argocd -o json"),
  "pods": [{"ns": p["metadata"]["namespace"], "name": p["metadata"]["name"],
            "images": [c["image"] for c in p["spec"]["containers"]],
            "restarts": sum(c.get("restartCount", 0) for c in p.get("status", {}).get("containerStatuses", []))} for p in pods],
  "mem_total_kib": int(open("/proc/meminfo").read().split()[1]),
}, indent=1))
PY' | save "$tag.info.json"
  rroot "$vm" "dmesg | grep -iE 'out of memory|oom-kill|killed process' || true" | save "$tag.oom.txt"
  host_snap | save "$tag.host.end.json"
  [[ -n "${KEEP_VMS:-}" ]] || "$VM" down "$vm"
}

# ------------------------------------------------------------------ observability

# litek8s's built-in metrics, log search and alerts against k3s minimal +
# kube-prometheus-stack + Loki (single binary, filesystem) + Alloy, on
# identical VMs running the same workload (bench/obs/workload.yaml).
#   phase_obs k3s | litek8s
# Each VM first runs the workload without any observability (the
# baseline), then with it: warm-up, a measured window (memory at both ends,
# CPU over it, store growth on disk, samples ingested), then common queries.

obs_charts() { # -> dir with the three charts (pulled once, versions recorded)
  local dir="$HERE/.work/obs" helm=helm
  mkdir -p "$dir"
  command -v helm >/dev/null || helm="$(argo_tools)/$GUEST_ARCH/helm"
  local H=(env HELM_CACHE_HOME="$dir/helm/cache" HELM_CONFIG_HOME="$dir/helm/config" HELM_DATA_HOME="$dir/helm/data" "$helm")
  [[ -n "$(ls "$dir"/kube-prometheus-stack-*.tgz 2>/dev/null)" ]] || "${H[@]}" pull kube-prometheus-stack --repo https://prometheus-community.github.io/helm-charts ${KPS_CHART_VERSION:+--version "$KPS_CHART_VERSION"} -d "$dir" >&2
  [[ -n "$(ls "$dir"/loki-*.tgz 2>/dev/null)" ]] || "${H[@]}" pull loki --repo https://grafana.github.io/helm-charts ${LOKI_CHART_VERSION:+--version "$LOKI_CHART_VERSION"} -d "$dir" >&2
  [[ -n "$(ls "$dir"/alloy-*.tgz 2>/dev/null)" ]] || "${H[@]}" pull alloy --repo https://grafana.github.io/helm-charts ${ALLOY_CHART_VERSION:+--version "$ALLOY_CHART_VERSION"} -d "$dir" >&2
  echo "$dir"
}

obs_window() { # vm tag system label
  local vm="$1" tag="$2" sys="$3" label="$4" K="export KUBECONFIG=$5;"
  rroot "$vm" "$K obs.py storage $sys" | save "$tag.$label.storage0.json"
  [[ "$label" == noobs ]] || rroot "$vm" "$K obs.py tsdb $sys" | save "$tag.$label.tsdb0.json"
  rroot "$vm" "probe.py mem" | save "$tag.$label.mem0.json"
  host_snap | save "$tag.$label.host0.json"
  local secs="$OBS_WINDOW_SECS"; [[ "$label" == noobs ]] && secs="$IDLE_SECS"
  log "$tag: measuring $label for ${secs}s"
  rroot "$vm" "probe.py cpu $secs" | save "$tag.$label.cpu.json"
  host_snap | save "$tag.$label.host1.json"
  rroot "$vm" "probe.py mem" | save "$tag.$label.mem1.json"
  [[ "$label" == noobs ]] || rroot "$vm" "$K obs.py tsdb $sys" | save "$tag.$label.tsdb1.json"
  rroot "$vm" "$K obs.py storage $sys" | save "$tag.$label.storage1.json"
}

phase_obs() {
  local sys="$1" vm port tag kc
  local VM_MEM="$OBS_VM_MEM" VM_CPUS="$OBS_VM_CPUS"
  if [[ "$sys" == k3s ]]; then vm=obsk3s port="$PORT_OBSK3S" kc=/etc/rancher/k3s/k3s.yaml
  else vm=obslk port="$PORT_OBSLK" kc=/root/litek8s.kubeconfig; fi
  tag="obs-$sys"
  local tools charts; tools="$(argo_tools)"; charts="$(obs_charts)"
  host_snap | save "$tag.host.start.json"
  local bin=""; [[ "$sys" == litek8s ]] && bin="$(linux_binary)"
  log "$tag: preparing VM (${VM_MEM} MiB, ${VM_CPUS} vCPU)"
  vm_prepare "$vm" "$port"
  local a="$GUEST_ARCH"
  "$VM" scp "$vm" "$tools/$a/helm" /tmp/helm
  "$VM" scp "$vm" "$tools/$a/kubectl" /tmp/kubectl
  "$VM" scp "$vm" "$HERE/node/obs.py" /tmp/obs.py
  rroot "$vm" 'install -m 755 /tmp/helm /tmp/kubectl /tmp/obs.py /usr/local/bin/ && mkdir -p /root/obs'
  for f in workload.yaml kps-values.yaml loki-values.yaml alloy-values.yaml; do "$VM" scp "$vm" "$HERE/obs/$f" "/tmp/$f"; done
  for f in "$charts"/*.tgz; do "$VM" scp "$vm" "$f" "/tmp/$(basename "$f")"; done
  rroot "$vm" 'mv /tmp/*.yaml /tmp/*.tgz /root/obs/'
  local K="export KUBECONFIG=$kc; cd /root/obs;"
  local disk0; disk0="$(disk_used_kib "$vm")"

  if [[ "$sys" == k3s ]]; then
    log "$tag: installing k3s ${K3S_VERSION:-stable} (minimal, with local-path storage for Loki)"
    rroot "$vm" "curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION='$K3S_VERSION' sh -s - --disable=traefik,servicelb,metrics-server >/dev/null"
  else
    log "$tag: installing crun + litek8s apiserver (observability off for the baseline)"
    rroot "$vm" 'export DEBIAN_FRONTEND=noninteractive
      for i in 1 2 3 4 5; do apt-get update -qq && break; sleep 10; done
      apt-get install -y -qq crun iptables >/dev/null'
    "$VM" scp "$vm" "$bin" /tmp/litek8s
    rroot "$vm" 'install -m 755 /tmp/litek8s /usr/local/bin/litek8s && rm /tmp/litek8s
      mkdir -p /var/lib/litek8s
      systemd-run --unit=litek8s --quiet /usr/local/bin/litek8s apiserver --listen 127.0.0.1:6443         --data-dir /var/lib/litek8s/apiserver --kubeconfig /root/litek8s.kubeconfig --runtime crun --no-observability
      for i in $(seq 1 100); do curl -sf '"$KUBE_URL"'/readyz >/dev/null && break; sleep 0.1; done'
  fi
  local wait_pods='sleep 5; kubectl wait --for=condition=Ready pod -A --all --field-selector=status.phase!=Succeeded --timeout=600s >/dev/null; kubectl get pods -A --no-headers'
  rroot "$vm" "$K"'for i in $(seq 1 120); do kubectl get nodes 2>/dev/null | grep -q " Ready" && break; sleep 2; done
    kubectl apply -f workload.yaml >/dev/null; '"$wait_pods" >"$OUT/$tag.noobs.pods.txt"
  log "$tag: workload only: settle ${SETTLE_SECS}s"
  sleep "$SETTLE_SECS"
  obs_window "$vm" "$tag" "$sys" noobs "$kc"

  local disk1; disk1="$(disk_used_kib "$vm")"
  if [[ "$sys" == k3s ]]; then
    log "$tag: helm install kube-prometheus-stack, loki, alloy"
    local t0; t0=$(date +%s)
    rroot "$vm" "$K"'kubectl create namespace monitoring >/dev/null
      helm install kube-prometheus-stack kube-prometheus-stack-*.tgz -n monitoring -f kps-values.yaml --wait --timeout 15m >/dev/null
      helm install loki loki-*.tgz -n monitoring -f loki-values.yaml --wait --timeout 15m >/dev/null
      helm install alloy alloy-*.tgz -n monitoring -f alloy-values.yaml --wait --timeout 10m >/dev/null; '"$wait_pods" >"$OUT/$tag.obs.pods.txt"
    echo "{"install_secs": $(( $(date +%s) - t0 ))}" | save "$tag.install.json"
  else
    log "$tag: restarting the API server with observability on (pods keep running)"
    local t0; t0=$(date +%s)
    rroot "$vm" 'systemctl stop litek8s; sleep 1
      systemd-run --unit=litek8s --quiet /usr/local/bin/litek8s apiserver --listen 127.0.0.1:6443         --data-dir /var/lib/litek8s/apiserver --kubeconfig /root/litek8s.kubeconfig --runtime crun
      for i in $(seq 1 100); do curl -sf '"$KUBE_URL"'/readyz >/dev/null && break; sleep 0.1; done'
    rroot "$vm" "$K $wait_pods" >"$OUT/$tag.obs.pods.txt"
    echo "{"install_secs": $(( $(date +%s) - t0 ))}" | save "$tag.install.json"
  fi
  local disk2; disk2="$(disk_used_kib "$vm")"
  echo "{"base_kib": $disk0, "with_workload_kib": $disk1, "with_observability_kib": $disk2}" | save "$tag.disk.json"

  log "$tag: warm-up ${OBS_WARMUP_SECS}s"
  sleep "$OBS_WARMUP_SECS"
  obs_window "$vm" "$tag" "$sys" obs "$kc"
  if [[ "$sys" == k3s ]]; then
    # Loki keeps recent chunks in memory: flush them to count their size.
    rroot "$vm" "$K obs.py loki-flush; sleep 30; obs.py storage k3s" | save "$tag.obs.storage-flushed.json"
  fi
  log "$tag: query latency ($OBS_QUERY_RUNS runs each)"
  rroot "$vm" "$K obs.py queries $sys $OBS_QUERY_RUNS" | save "$tag.queries.json"
  rroot "$vm" "$K"' python3 - <<'"'PY'"'
import json, subprocess
run = lambda *a: subprocess.run(a, capture_output=True, text=True).stdout.strip()
pods = json.loads(run("kubectl", "get", "pods", "-A", "-o", "json"))["items"]
print(json.dumps({
  "server_version": json.loads(run("kubectl", "version", "-o", "json")).get("serverVersion", {}).get("gitVersion"),
  "k3s_version": run("sh", "-c", "k3s --version 2>/dev/null | head -1"),
  "crun_version": run("sh", "-c", "crun --version 2>/dev/null | head -1"),
  "charts": run("sh", "-c", "helm list -A -o json 2>/dev/null"),
  "pods": [{"ns": p["metadata"]["namespace"], "name": p["metadata"]["name"],
            "images": [c["image"] for c in p["spec"]["containers"]],
            "restarts": sum(c.get("restartCount", 0) for c in p.get("status", {}).get("containerStatuses", []))} for p in pods],
  "mem_total_kib": int(open("/proc/meminfo").read().split()[1]),
}, indent=1))
PY' | save "$tag.info.json"
  rroot "$vm" "dmesg | grep -iE 'out of memory|oom-kill|killed process' || true" | save "$tag.oom.txt"
  host_snap | save "$tag.host.end.json"
  [[ -n "${KEEP_VMS:-}" ]] || "$VM" down "$vm"
}

# ------------------------------------------------------------------ main

{
  echo "{\"started\": \"$(date -u +%FT%TZ)\", \"idle_secs\": $IDLE_SECS, \"settle_secs\": $SETTLE_SECS,"
  echo " \"ttr_runs\": $TTR_RUNS, \"rpc_runs\": $RPC_RUNS, \"vm_mem_mib\": $VM_MEM, \"vm_cpus\": $VM_CPUS,"
  echo " \"argo_vm_mem_mib\": $ARGO_VM_MEM, \"argo_vm_cpus\": $ARGO_VM_CPUS, \"argo_runs\": $ARGO_RUNS,"
  echo " \"argo_chart_version\": \"$ARGO_CHART_VERSION\", \"helm\": \"$HELM_VERSION\", \"kubectl\": \"$KUBECTL_VERSION\","
  echo " \"host\": \"$(sysctl -n machdep.cpu.brand_string 2>/dev/null || sed -n 's/^model name[^:]*: //p' /proc/cpuinfo 2>/dev/null | head -1 || uname -m)\","
  echo " \"host_cpus\": $(getconf _NPROCESSORS_ONLN), \"guest_arch\": \"$GUEST_ARCH\","
  echo " \"litek8s_commit\": \"${LITEK8S_COMMIT:-$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null)}\", \"phases\": \"${PHASES[*]}\"}"
} >"$OUT/env.json"

for p in "${PHASES[@]}"; do
  case "$p" in
    litek8s) phase_litek8s ;;
    k3s) phase_k3s k3s "$PORT_K3S" "" ;;
    k3s-minimal) phase_k3s k3smin "$PORT_K3SMIN" "--disable=traefik,servicelb,metrics-server,local-storage" ;;
    rpc) phase_rpc ;;
    kube) phase_kube docker ;;
    kube-crun) phase_kube crun ;;
    argocd-k3s) phase_argocd k3s ;;
    argocd-litek8s) phase_argocd litek8s ;;
    obs-k3s) phase_obs k3s ;;
    obs-litek8s) phase_obs litek8s ;;
    *) echo "unknown phase $p" >&2; exit 2 ;;
  esac
done
[[ " ${PHASES[*]} " == *" rpc "* || -n "${KEEP_VMS:-}" ]] || "$VM" down lk >/dev/null 2>&1 || true
log "done: $OUT"
python3 "$HERE/report.py" "$OUT" || true
if ls "$OUT"/argocd-*.install-cold.json >/dev/null 2>&1; then python3 "$HERE/argocd_report.py" "$OUT" || true; fi
