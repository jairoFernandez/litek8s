#!/usr/bin/env bash
# macOS variant of bench/dev-bench.sh: litek8s dev (Virtualization.framework
# VM) vs kind and k3d on the Docker the Mac already has (Docker Desktop or
# OrbStack, used as they are). Same protocol; run from the repo root:
#   KUBECONFIG=/tmp/x.kubeconfig bench/dev-bench-mac.sh <litek8s|kind|k3d> [runs]
#
#   up_s / pod_s   fresh cluster each run (`dev reset` first, like a new kind
#                  cluster), with nginx:alpine loaded from an archive
#   warm_up_s      litek8s only: `dev down` + `dev up` (objects and images kept)
#   mem_mib        litek8s: physical footprint of the framework's VM process
#                  (what Activity Monitor shows) and, inside the VM, used memory
#                  (MemTotal - MemAvailable); kind/k3d: node containers'
#                  memory.current - inactive_file inside the Docker VM (the
#                  Docker VM itself is not counted)
#   cpu_pct        over 60 s idle: litek8s VM process CPU; kind/k3d node
#                  containers' cgroup CPU (% of one CPU)
#   disk_mib       litek8s: data dir (sparse disk's allocated blocks) + guest
#                  kernel/root image + binary; kind/k3d: node image + writable layers
set -euo pipefail
TOOL="$1"; RUNS="${2:-3}"
LK="${LITEK8S:-$PWD/target/release/litek8s}"
KIND_IMAGE="${KIND_IMAGE:-kindest/node:v1.34.0}"
K3S_IMAGE="${K3S_IMAGE:-rancher/k3s:v1.33.4-k3s1}"
ARCHIVE="${ARCHIVE:-/tmp/litek8s-bench-nginx-alpine.tar}"
HOME_DEV="$HOME/Library/Application Support/dev.litek8s/dev"
[[ -d "$HOME_DEV" || ! -d "$HOME/Library/Application Support/dev.civojairo.litek8s/dev" ]] || HOME_DEV="$HOME/Library/Application Support/dev.civojairo.litek8s/dev"
now() { python3 -c 'import time;print(time.time())'; }
[[ -f "$ARCHIVE" ]] || { docker pull -q nginx:alpine >/dev/null; docker save --platform linux/arm64 nginx:alpine -o "$ARCHIVE"; }

up() {
  case $TOOL in
    litek8s) "$LK" dev up >/dev/null ;;
    kind) kind create cluster --name devbench --image "$KIND_IMAGE" --wait 5m >/dev/null 2>&1 ;;
    k3d) k3d cluster create devbench --image "$K3S_IMAGE" --wait --timeout 5m --no-lb --api-port 127.0.0.1:26550 --k3s-arg '--disable=traefik,metrics-server@server:0' --k3s-arg '--kubelet-arg=eviction-hard=nodefs.available<1%,imagefs.available<1%@server:0' >/dev/null 2>&1 ;;
  esac
  kubectl wait --for=condition=Ready node --all --timeout=300s >/dev/null
}
down() {
  case $TOOL in
    litek8s) "$LK" dev reset >/dev/null ;;
    kind) kind delete cluster --name devbench >/dev/null 2>&1 || true ;;
    k3d) k3d cluster delete devbench >/dev/null 2>&1 || true ;;
  esac
}
load() {
  case $TOOL in
    litek8s) "$LK" dev load "$ARCHIVE" >/dev/null ;;
    kind) kind load image-archive "$ARCHIVE" --name devbench >/dev/null 2>&1 ;;
    k3d) k3d image import "$ARCHIVE" -c devbench >/dev/null 2>&1 ;;
  esac
}
nodes() { docker ps -q --filter "name=devbench"; }
container_mem() { # MiB over all node containers
  local t=0
  for id in $(nodes); do
    t=$((t + $(docker exec "$id" sh -c 'echo $(( $(cat /sys/fs/cgroup/memory.current) - $(awk "/^inactive_file /{print \$2}" /sys/fs/cgroup/memory.stat) ))')))
  done
  python3 -c "print(round($t/1048576,1))"
}
container_cpu() { local t=0; for id in $(nodes); do t=$((t + $(docker exec "$id" awk '/^usage_usec /{print $2}' /sys/fs/cgroup/cpu.stat))); done; echo $t; }
helper_pid() { python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["helper_pid"])' "$HOME_DEV/dev.json"; }
vm_footprint() { "$LK" dev status | awk '/memory:/{print $2, $3}'; }
vm_cpu_s() { ps -o time= -p "$(helper_pid)" | awk -F'[:.]' '{print ($1*60)+$2+($3/100)}'; }
guest_used() {
  kubectl run memprobe --rm -i --restart=Never --image=nginx:alpine --image-pull-policy=IfNotPresent --overrides='{"spec":{"hostPID":true}}' --command -- \
    awk '/MemTotal/{t=$2}/MemAvailable/{a=$2}END{printf "%.1f\n", (t-a)/1024}' /proc/meminfo 2>/dev/null | grep -m1 -E '^[0-9.]+$' || true
}

down
ups=(); pods=()
for i in $(seq 1 "$RUNS"); do
  t0=$(now); up; t1=$(now); load
  kubectl create deployment web --image=nginx:alpine >/dev/null
  kubectl patch deployment web --type=json -p '[{"op":"replace","path":"/spec/template/spec/containers/0/imagePullPolicy","value":"IfNotPresent"}]' >/dev/null
  kubectl wait --for=condition=available deployment/web --timeout=300s >/dev/null
  t2=$(now)
  ups+=("$(python3 -c "print(round($t1-$t0,2))")"); pods+=("$(python3 -c "print(round($t2-$t0,2))")")
  [[ $i -lt $RUNS ]] && down
done
warm=()
if [[ $TOOL == litek8s ]]; then
  for i in 1 2 3; do "$LK" dev down >/dev/null; t0=$(now); "$LK" dev up >/dev/null; kubectl wait --for=condition=available deployment/web --timeout=120s >/dev/null; warm+=("$(python3 -c "print(round($(now)-$t0,2))")"); done
fi
sleep 60
if [[ $TOOL == litek8s ]]; then
  c0=$(vm_cpu_s); sleep 60; c1=$(vm_cpu_s); cpu=$(python3 -c "print(round(($c1-$c0)/60*100,2))")
  mem="$(vm_footprint) footprint, $(guest_used) MiB used in the guest"
else
  c0=$(container_cpu); sleep 60; c1=$(container_cpu); cpu=$(python3 -c "print(round(($c1-$c0)/600000,2))")
  mem="$(container_mem) MiB"
fi
kubectl scale deployment web --replicas=5 >/dev/null
kubectl wait --for=condition=available deployment/web --timeout=300s >/dev/null
sleep 30
if [[ $TOOL == litek8s ]]; then mem5="$(vm_footprint) footprint, $(guest_used) MiB used in the guest"; else mem5="$(container_mem) MiB"; fi
case $TOOL in
  litek8s) disk=$( (du -sm "$HOME_DEV" "$HOME_DEV"/../vm/{Image,initramfs.gz,initrd.gz,root.img} "$LK" 2>/dev/null || true) | awk '{s+=$1}END{print s}') ;;
  *) img=$KIND_IMAGE; [[ $TOOL == k3d ]] && img=$K3S_IMAGE
     disk=$(python3 -c "print(round(($(docker image inspect -f '{{.Size}}' "$img") + $(for id in $(nodes); do docker inspect -s -f '{{.SizeRw}}' "$id"; done | paste -sd+ - | bc))/1048576))") ;;
esac
down
python3 - "$TOOL" "${ups[*]}" "${pods[*]}" "${warm[*]:-}" "$mem" "$cpu" "$mem5" "$disk" <<'EOF'
import json, statistics, sys
tool, ups, pods, warm, mem, cpu, mem5, disk = sys.argv[1:]
f = lambda s: [float(x) for x in s.split()]
out = {"tool": tool, "up_s": f(ups), "up_s_median": statistics.median(f(ups)), "pod_s": f(pods),
       "pod_s_median": statistics.median(f(pods)), "mem": mem, "cpu_pct": float(cpu), "mem5": mem5, "disk_mib": float(disk)}
if warm.strip():
    out["warm_up_s"] = f(warm)
print(json.dumps(out))
EOF
