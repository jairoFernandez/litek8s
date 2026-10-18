#!/usr/bin/env bash
# Local dev cluster benchmark: litek8s dev vs kind vs k3d, same protocol.
# Runs ON the machine under test (copy it there): bench/dev-bench.sh <litek8s|kind|k3d> [runs]
#
# Per run, from a stopped cluster with its node image and nginx:alpine already
# on the machine (pulled once, not timed):
#   up_s         `dev up` / `kind create cluster --wait` / `k3d cluster create --wait`
#                until the node is Ready
#   pod_s        up + loading nginx:alpine into the cluster + a Deployment until
#                its pod is Ready (the "first Ready pod" a developer waits for)
# After the last run, with the cluster idle for 60 s:
#   mem_mib      cgroup memory.current - inactive_file of everything the cluster
#                runs (litek8s: its systemd scope; kind/k3d: the node containers),
#                the number `docker stats` shows
#   cpu_pct      CPU of the same cgroups over those 60 s (% of one CPU)
#   mem5_mib     the same with 5 nginx replicas
#   disk_mib     on-disk footprint: litek8s binary + data dir; kind/k3d node
#                images + their containers' writable layers and volumes
# Prints one JSON object.
set -euo pipefail
TOOL="$1"; RUNS="${2:-3}"
LK="${LITEK8S:-$HOME/litek8s}"
KUBECTL="${KUBECTL:-$HOME/kubectl}"
KIND_IMAGE="${KIND_IMAGE:-kindest/node:v1.34.0}"
K3S_IMAGE="${K3S_IMAGE:-rancher/k3s:v1.33.4-k3s1}"
now() { date +%s.%N; }
k() { "$KUBECTL" "$@"; }

up() {
  case $TOOL in
    litek8s) "$LK" dev up >/dev/null ;;
    kind) "$HOME/kind" create cluster --name bench --image "$KIND_IMAGE" --wait 5m >/dev/null 2>&1 ;;
    k3d) "$HOME/k3d" cluster create bench --image "$K3S_IMAGE" --wait --timeout 5m --no-lb --k3s-arg '--disable=traefik,metrics-server@server:0' >/dev/null 2>&1 ;;
  esac
  k wait --for=condition=Ready node --all --timeout=300s >/dev/null
}

down() {
  case $TOOL in
    litek8s) "$LK" dev reset >/dev/null ;;
    kind) "$HOME/kind" delete cluster --name bench >/dev/null 2>&1 || true ;;
    k3d) "$HOME/k3d" cluster delete bench >/dev/null 2>&1 || true ;;
  esac
}

load() {
  case $TOOL in
    litek8s) "$LK" dev load /tmp/nginx-alpine.tar >/dev/null ;;
    kind) "$HOME/kind" load image-archive /tmp/nginx-alpine.tar --name bench >/dev/null 2>&1 ;;
    k3d) "$HOME/k3d" image import /tmp/nginx-alpine.tar -c bench >/dev/null 2>&1 ;;
  esac
}

cgroups() {
  case $TOOL in
    litek8s) python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.local/share/litek8s/dev/dev.json")))["cgroup"])' ;;
    kind|k3d)
      for id in $(docker ps -q --filter "name=bench"); do
        p=$(docker inspect -f '{{.Id}}' "$id")
        find /sys/fs/cgroup -maxdepth 4 -type d -name "*$p*" | head -1
      done ;;
  esac
}

mem_mib() {
  local total=0
  for cg in $(cgroups); do
    cur=$(cat "$cg/memory.current"); inact=$(awk '/^inactive_file /{print $2}' "$cg/memory.stat")
    total=$((total + cur - inact))
  done
  python3 -c "print(round($total/1048576,1))"
}

cpu_usec() {
  local total=0
  for cg in $(cgroups); do total=$((total + $(awk '/^usage_usec /{print $2}' "$cg/cpu.stat"))); done
  echo $total
}

disk_mib() {
  case $TOOL in
    litek8s) sudo du -scm "$LK" "$HOME/.local/share/litek8s/dev" | tail -1 | cut -f1 ;;
    kind|k3d)
      local img=$KIND_IMAGE; [[ $TOOL == k3d ]] && img=$K3S_IMAGE
      local isz; isz=$(docker image inspect -f '{{.Size}}' "$img")
      local csz=0
      for id in $(docker ps -q --filter "name=bench"); do
        csz=$((csz + $(docker inspect -s -f '{{.SizeRw}}' "$id")))
      done
      local vsz; vsz=$(sudo du -scb /var/lib/docker/volumes 2>/dev/null | tail -1 | cut -f1)
      python3 -c "print(round(($isz+$csz+$vsz)/1048576))" ;;
  esac
}

down
ups=(); pods=()
for i in $(seq 1 "$RUNS"); do
  t0=$(now); up; t1=$(now)
  load
  k create deployment web --image=nginx:alpine >/dev/null
  k patch deployment web --type=json -p '[{"op":"replace","path":"/spec/template/spec/containers/0/imagePullPolicy","value":"IfNotPresent"}]' >/dev/null
  k wait --for=condition=available deployment/web --timeout=300s >/dev/null
  t2=$(now)
  ups+=("$(python3 -c "print(round($t1-$t0,2))")"); pods+=("$(python3 -c "print(round($t2-$t0,2))")")
  [[ $i -lt $RUNS ]] && down
done
sleep 60
c0=$(cpu_usec); sleep 60; c1=$(cpu_usec)
mem=$(mem_mib); cpu=$(python3 -c "print(round(($c1-$c0)/600000,2))")
k scale deployment web --replicas=5 >/dev/null
k wait --for=condition=available deployment/web --timeout=300s >/dev/null
sleep 30; mem5=$(mem_mib)
disk=$(disk_mib)
down
python3 - "$TOOL" "${ups[*]}" "${pods[*]}" "$mem" "$cpu" "$mem5" "$disk" <<'EOF'
import json, statistics, sys
tool, ups, pods, mem, cpu, mem5, disk = sys.argv[1:]
ups = [float(x) for x in ups.split()]; pods = [float(x) for x in pods.split()]
print(json.dumps({"tool": tool, "up_s": ups, "up_s_median": statistics.median(ups), "pod_s": pods,
                  "pod_s_median": statistics.median(pods), "mem_mib": float(mem), "cpu_pct": float(cpu),
                  "mem5_mib": float(mem5), "disk_mib": float(disk)}))
EOF
