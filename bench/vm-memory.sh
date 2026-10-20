#!/usr/bin/env bash
# What macOS charges the litek8s dev VM through a day's work, from a fresh
# (deleted) cluster: idle, with Argo CD, after a big image pull and five
# idle minutes, and a sudden 1 GiB allocation in a pod (how long it takes,
# whether anything is OOM-killed). Run from the repo root on a Mac:
#
#   LITEK8S=<binary> LITEK8S_DEV_VM_ASSETS=<guest dir> bench/vm-memory.sh <label>
#
# Isolate it from a cluster you use (LITEK8S_DEV_HOME, LITEK8S_DOCKER_SOCKET,
# KUBECONFIG, DOCKER_CONFIG, LITEK8S_CONFIG): it deletes the cluster's data
# at the start and the end. Prints one line per phase:
#   <label> <phase> footprint=<MiB> limit=<MiB> guest_used=<MiB> cache=<MiB>
# footprint: the framework's VM process (Activity Monitor's figure);
# limit: what the guest may use now (the balloon's ceiling); guest_used:
# MemTotal - MemFree - balloon; cache: Cached + Buffers.
set -euo pipefail
LABEL="$1"
LK="${LITEK8S:-$PWD/target/release/litek8s}"
ARGO="${ARGO:-https://raw.githubusercontent.com/argoproj/argo-cd/v3.1.9/manifests/install.yaml}"
BIG="${BIG:-python:3.13}"
IDLE="${IDLE:-300}"
HOME_DEV="${LITEK8S_DEV_HOME:?set LITEK8S_DEV_HOME (an isolated dev home)}"
docker_host="unix://${LITEK8S_DOCKER_SOCKET:?set LITEK8S_DOCKER_SOCKET}"

# env.json: the shared VM (named clusters); dev.json before them.
helper() { python3 -c "import json,os,sys;print(json.load(open(next(p for p in sys.argv[1:] if os.path.exists(p))))['helper_pid'])" "$HOME_DEV/env.json" "$HOME_DEV/dev.json"; }
footprint_mib() { footprint "$(helper)" | awk '/phys_footprint:/{v=$2; u=$3; if (u=="GB") v*=1024; if (u=="KB") v/=1024; printf "%d", v}'; }
guest() { kubectl exec probe -- sh -c 'cat /proc/meminfo; grep balloon_ /proc/vmstat'; }
sample() {
  local fp g
  fp=$(footprint_mib)
  g=$(guest | awk '/^MemTotal:/{t=$2} /^MemFree:/{f=$2} /^Cached:/{c=$2} /^Buffers:/{b=$2} /^balloon_inflate/{i=$2} /^balloon_deflate/{d=$2}
    END{bal=(i-d)*4; printf "limit=%d guest_used=%d cache=%d", 4096 - bal/1024, (t-f-bal)/1024, (c+b)/1024}')
  echo "$LABEL $1 footprint=$fp $g"
}

"$LK" dev down >/dev/null 2>&1 || true
"$LK" dev delete >/dev/null 2>&1 || "$LK" dev reset >/dev/null 2>&1 || true
"$LK" dev up --port "${PORT:-16550}" --cpus 4 --memory 4096 >/dev/null
cat <<'EOF' | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: probe}
spec:
  containers:
  - name: p
    image: python:3.13-alpine
    command: ["sleep", "infinity"]
    securityContext: {privileged: true}
EOF
kubectl wait --for=condition=Ready pod/probe --timeout=300s >/dev/null
sleep 120
sample idle

kubectl create namespace argocd >/dev/null
kubectl apply -n argocd --server-side -f "$ARGO" >/dev/null
kubectl -n argocd rollout status deploy --timeout=600s >/dev/null
kubectl -n argocd rollout status statefulset --timeout=600s >/dev/null
sleep 120
sample argocd

DOCKER_HOST="$docker_host" docker pull -q "$BIG" >/dev/null
sample pulled
sleep "$IDLE"
sample "idle${IDLE}s"

cat > /tmp/litek8s-vm-memory-alloc.py <<'EOF'
import time
t = time.time(); chunks = []
for i in range(1024):
    b = bytearray(1 << 20)
    for j in range(0, 1 << 20, 4096): b[j] = 1
    chunks.append(b)
print(f"{time.time() - t:.2f}", flush=True)
time.sleep(5)
EOF
kubectl cp /tmp/litek8s-vm-memory-alloc.py probe:/alloc.py >/dev/null
t0=$(python3 -c 'import time;print(time.time())')
secs=$(kubectl exec probe -- python3 /alloc.py)
total=$(python3 -c "import time;print(f'{time.time()-$t0:.1f}')")
ooms=$(kubectl exec probe -- sh -c 'dmesg | grep -c "oom-kill" || true')
ready=$(kubectl get nodes --no-headers | awk '{print $2}')
echo "$LABEL burst alloc_1GiB_s=$secs exec_total_s=$total oom_kills=$ooms node=$ready"
sample burst

"$LK" dev down >/dev/null 2>&1 || true
"$LK" dev delete >/dev/null 2>&1 || "$LK" dev reset >/dev/null 2>&1 || true
