#!/usr/bin/env bash
# Soak test of a multi-node litek8s cluster (bench/cluster.sh): runs a steady
# workload with churn for a long time and samples what every litek8s process
# costs, to find leaks and unbounded growth.
#
#   bench/soak.sh [--duration 24h] [--sample 60] [--rollout-every 5m]
#                 [--kill-every 2m] [--node-every 1h] [--out DIR]
#
# Workload (namespace soak): a 6-replica nginx Deployment behind a Service, a
# DaemonSet, a StatefulSet with a volume, a CronJob every minute, a prober
# that hits the Service through DNS once a second and logs the result, and
# ConfigMap updates. Churn:
#   - rollout restart of the Deployment every --rollout-every
#   - a random pod of the workload deleted every --kill-every
#   - every --node-every one worker node goes down and comes back; the
#     outages alternate: a hard power-off for 8 min (past the 40 s grace and
#     the 300 s default tolerations, so pods are evicted and rescheduled,
#     then the node rejoins), a power-off for 60 s (back before eviction:
#     keeps its pods), a 2-min freeze (SIGSTOP of the VM: a laptop that
#     sleeps), and a restart of the agent only.
# Every --sample seconds, per node: RSS (PSS where readable), CPU time and
# count of every litek8s process (apiserver/agent, crun shims), node memory;
# from /metrics: objects, WAL size/records/compactions, watch count, event
# history, agent connections, request latency per verb; plus timed GETs on
# the server itself, the prober's success rate and pod/restart counts.
#
# Output: bench/results/<date>-soak/{samples.jsonl,events.log}; summarize
# with bench/soak_report.py <dir>. Stop early with Ctrl-C (the report works
# on whatever was sampled).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
VM="$HERE/vm.sh"
export KUBECONFIG="${KUBECONFIG_SOAK:-$("$HERE/cluster.sh" kubeconfig)}"

secs() { # 90s 5m 2h 1d -> seconds
  local v="$1"
  case "$v" in
    *d) echo $(( ${v%d} * 86400 )) ;; *h) echo $(( ${v%h} * 3600 )) ;;
    *m) echo $(( ${v%m} * 60 )) ;; *s) echo "${v%s}" ;; *) echo "$v" ;;
  esac
}

DURATION=$(secs 24h); SAMPLE=60; ROLLOUT=$(secs 5m); KILL=$(secs 2m); NODE=$(secs 1h)
OUT="$HERE/results/$(date +%Y-%m-%d)-soak"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --duration) DURATION=$(secs "$2"); shift 2 ;;
    --sample) SAMPLE=$(secs "$2"); shift 2 ;;
    --rollout-every) ROLLOUT=$(secs "$2"); shift 2 ;;
    --kill-every) KILL=$(secs "$2"); shift 2 ;;
    --node-every) NODE=$(secs "$2"); shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$OUT"
SAMPLES="$OUT/samples.jsonl"
EVENTS="$OUT/events.log"
NODES=$(cat "$HERE/.work/cluster/nodes" 2>/dev/null || echo 3)

event() { echo "$(date -u +%FT%TZ) $*" | tee -a "$EVENTS" >&2; }
kc() { perl -e 'alarm shift; exec @ARGV' 60 kubectl "$@"; }

deploy_workload() {
  kc create namespace soak >/dev/null 2>&1 || true
  kc -n soak apply -f - >/dev/null <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: 6
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: web
        image: nginx:alpine
        ports: [{containerPort: 80}]
        readinessProbe: {httpGet: {path: /, port: 80}, periodSeconds: 5}
        volumeMounts: [{name: conf, mountPath: /etc/soak}]
      volumes: [{name: conf, configMap: {name: soak-conf}}]
---
apiVersion: v1
kind: Service
metadata: {name: web}
spec:
  selector: {app: web}
  ports: [{port: 80}]
---
apiVersion: v1
kind: ConfigMap
metadata: {name: soak-conf}
data: {generation: "0"}
---
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: node-echo}
spec:
  selector: {matchLabels: {app: node-echo}}
  template:
    metadata: {labels: {app: node-echo}}
    spec:
      containers:
      - name: echo
        image: busybox:1.36
        command: [sh, -c, 'mkdir -p /www && hostname > /www/index.html && exec httpd -f -p 8080 -h /www']
---
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: db}
spec:
  serviceName: db
  replicas: 2
  selector: {matchLabels: {app: db}}
  template:
    metadata: {labels: {app: db}}
    spec:
      containers:
      - name: db
        image: busybox:1.36
        command: [sh, -c, 'date >> /data/boots; exec sleep 1000000']
        volumeMounts: [{name: data, mountPath: /data}]
  volumeClaimTemplates:
  - metadata: {name: data}
    spec: {accessModes: [ReadWriteOnce], resources: {requests: {storage: 64Mi}}}
---
apiVersion: batch/v1
kind: CronJob
metadata: {name: tick}
spec:
  schedule: "* * * * *"
  successfulJobsHistoryLimit: 3
  failedJobsHistoryLimit: 1
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers:
          - {name: tick, image: busybox:1.36, command: [sh, -c, 'wget -qO- -T 5 http://web.soak.svc.cluster.local/ >/dev/null && echo ok']}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: prober}
spec:
  replicas: 1
  selector: {matchLabels: {app: prober}}
  template:
    metadata: {labels: {app: prober}}
    spec:
      containers:
      - name: prober
        image: busybox:1.36
        command: [sh, -c, 'while true; do if wget -qO /dev/null -T 2 http://web/; then echo ok; else echo fail; fi; sleep 1; done']
EOF
  for d in deployment/web deployment/prober daemonset/node-echo statefulset/db; do
    kc -n soak rollout status "$d" --timeout=300s >/dev/null || event "warning: $d not ready yet"
  done
}

# One JSON sample of a node's litek8s processes and memory, taken on the node.
NODE_SAMPLER='
python3 - <<"PY"
import json, os, re
hz = os.sysconf("SC_CLK_TCK")
procs = {}
for pid in os.listdir("/proc"):
    if not pid.isdigit():
        continue
    try:
        cmd = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
        if not cmd or not cmd[0].endswith(b"litek8s"):
            continue
        role = cmd[1].decode() if len(cmd) > 1 else "?"
        stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        cpu = (int(stat[11]) + int(stat[12])) / hz
        rss = pss = 0
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1])
        try:
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Pss:"):
                    pss = int(line.split()[1])
        except OSError:
            pss = rss
        fds = len(os.listdir(f"/proc/{pid}/fd"))
        threads = int(open(f"/proc/{pid}/status").read().split("Threads:")[1].split()[0])
    except (OSError, IndexError, ValueError):
        continue
    p = procs.setdefault(role, {"count": 0, "rss_kib": 0, "pss_kib": 0, "cpu_s": 0.0, "fds": 0, "threads": 0})
    p["count"] += 1; p["rss_kib"] += rss; p["pss_kib"] += pss; p["cpu_s"] += cpu; p["fds"] += fds; p["threads"] += threads
mem = {l.split(":")[0]: int(l.split()[1]) for l in open("/proc/meminfo")}
du = 0
for d in ("/var/lib/litek8s/apiserver/objects.wal",):
    try: du = os.path.getsize(d)
    except OSError: pass
print(json.dumps({"procs": procs, "mem_available_kib": mem.get("MemAvailable", 0), "wal_file_bytes": du,
                  "load1": float(open("/proc/loadavg").read().split()[0])}))
PY'

sample_node() { # name -> json or {}
  perl -e 'alarm 20; exec @ARGV' "$VM" ssh "$1" "$NODE_SAMPLER" 2>/dev/null || echo '{}'
}

metrics() {
  perl -e 'alarm 20; exec @ARGV' kubectl get --raw /metrics 2>/dev/null | python3 -c '
import json, sys
out = {}
for line in sys.stdin:
    if line.startswith("#") or not line.strip():
        continue
    name, _, value = line.rpartition(" ")
    if "_bucket{" in name:
        continue
    out[name] = float(value)
print(json.dumps(out))' 2>/dev/null || echo '{}'
}

# Latency of a few reads, timed on the server (no tunnel in the way).
api_latency() {
  perl -e 'alarm 30; exec @ARGV' "$VM" ssh lk1 '
for p in /api/v1/nodes /api/v1/namespaces/soak/pods /apis/apps/v1/namespaces/soak/deployments/web /api/v1/namespaces/soak/configmaps; do
  t=0; for i in 1 2 3; do x=$(curl -s -o /dev/null -w "%{time_total}" "http://127.0.0.1:6443$p"); t=$(python3 -c "print(max($t,$x))"); done
  echo "$p $t"
done' 2>/dev/null | python3 -c '
import json, sys
print(json.dumps({l.split()[0]: float(l.split()[1]) for l in sys.stdin if len(l.split()) == 2}))' 2>/dev/null || echo '{}'
}

cluster_state() {
  kc get pods -A -o json 2>/dev/null | python3 -c '
import json, sys
items = json.load(sys.stdin)["items"]
phases = {}
for p in items:
    ph = p["status"].get("phase", "?")
    if p["metadata"].get("deletionTimestamp"): ph = "Terminating"
    phases[ph] = phases.get(ph, 0) + 1
restarts = sum(c.get("restartCount", 0) for p in items for c in p["status"].get("containerStatuses") or [])
web = [p for p in items if p["metadata"].get("labels", {}).get("app") == "web" and p["metadata"]["namespace"] == "soak"]
ready = sum(1 for p in web if any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", [])))
print(json.dumps({"pods": len(items), "phases": phases, "restarts": restarts, "web_ready": ready}))' 2>/dev/null || echo '{}'
}

prober_stats() { # successes/failures over the last sample interval
  kc -n soak logs deploy/prober --since="${SAMPLE}s" 2>/dev/null | python3 -c '
import json, sys
lines = sys.stdin.read().split()
print(json.dumps({"ok": lines.count("ok"), "fail": lines.count("fail")}))' 2>/dev/null || echo '{}'
}

nodes_state() {
  kc get nodes -o json 2>/dev/null | python3 -c '
import json, sys
out = {}
for n in json.load(sys.stdin)["items"]:
    ready = [c["status"] for c in n["status"].get("conditions", []) if c["type"] == "Ready"]
    out[n["metadata"]["name"]] = {"ready": ready[0] if ready else "?", "taints": [t["key"] for t in n["spec"].get("taints", [])]}
print(json.dumps(out))' 2>/dev/null || echo '{}'
}

take_sample() {
  local t node parts=()
  t=$(date +%s)
  for i in $(seq 1 "$NODES"); do
    node="lk$i"
    parts+=("\"$node\": $(sample_node "$node")")
  done
  local IFS=,
  printf '{"t": %s, "nodes": {%s}, "metrics": %s, "latency": %s, "cluster": %s, "prober": %s, "node_status": %s}\n' \
    "$t" "${parts[*]}" "$(metrics)" "$(api_latency)" "$(cluster_state)" "$(prober_stats)" "$(nodes_state)" >>"$SAMPLES"
}

ROLLOUTS=0
churn_rollout() { # n; runs in the background so sampling stays on time
  local n="$1" start=$SECONDS
  kc -n soak patch configmap soak-conf -p "{\"data\":{\"generation\":\"$n\"}}" >/dev/null
  if kc -n soak rollout restart deployment/web >/dev/null && kc -n soak rollout status deployment/web --timeout=300s >/dev/null; then
    event "rollout $n done in $((SECONDS - start))s"
  else
    event "rollout $n did not finish in 300s"
  fi
}

churn_kill_pod() {
  local pod
  pod=$(kc -n soak get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | grep -E '^(web|node-echo|db|prober)-' | awk 'BEGIN{srand()} {a[NR]=$0} END{if (NR) print a[int(rand()*NR)+1]}')
  [[ -n "$pod" ]] && kc -n soak delete pod "$pod" --wait=false >/dev/null 2>&1 && event "deleted pod $pod"
}

OUTAGE=0
WORKER=""
OUTAGE_END=0
OUTAGE_KIND=""
start_outage() {
  local workers=() kinds=(kill-long kill-short freeze agent-restart)
  for i in $(seq 2 "$NODES"); do workers+=("lk$i"); done
  WORKER=${workers[$((OUTAGE % ${#workers[@]}))]}
  OUTAGE_KIND=${kinds[$((OUTAGE % 4))]}
  OUTAGE=$((OUTAGE + 1))
  case "$OUTAGE_KIND" in
    kill-long) "$VM" kill "$WORKER"; OUTAGE_END=$((SECONDS + 480)) ;;
    kill-short) "$VM" kill "$WORKER"; OUTAGE_END=$((SECONDS + 60)) ;;
    freeze) "$VM" pause "$WORKER"; OUTAGE_END=$((SECONDS + 120)) ;;
    agent-restart) "$VM" ssh "$WORKER" 'sudo systemctl restart litek8s-agent' >/dev/null 2>&1; OUTAGE_END=0 ;;
  esac
  event "outage $OUTAGE: $OUTAGE_KIND of $WORKER"
}

end_outage() {
  case "$OUTAGE_KIND" in
    kill-long|kill-short) "$VM" start "$WORKER" >/dev/null 2>&1 ;;
    freeze) "$VM" resume "$WORKER" ;;
  esac
  event "outage $OUTAGE over: $WORKER restored"
  OUTAGE_END=0
  # The VM's ssh port changed hands; the tunnel to lk1 is unaffected.
}

cleanup() {
  if ((OUTAGE_END > 0)); then end_outage; fi
  event "soak stopped after $(( SECONDS / 60 )) min"
  exit 0
}
trap cleanup INT TERM

event "soak start: duration ${DURATION}s, sample ${SAMPLE}s, rollout ${ROLLOUT}s, pod kill ${KILL}s, node outage ${NODE}s; $NODES nodes"
deploy_workload
event "workload ready"
next_sample=0; next_rollout=$ROLLOUT; next_kill=$KILL; next_node=$NODE
begin=$SECONDS
while (( SECONDS - begin < DURATION )); do
  now=$((SECONDS - begin))
  if (( now >= next_sample )); then take_sample; next_sample=$((next_sample + SAMPLE)); fi
  if (( OUTAGE_END > 0 && SECONDS >= OUTAGE_END )); then end_outage; fi
  if (( now >= next_node && OUTAGE_END == 0 )); then start_outage; next_node=$((next_node + NODE)); fi
  if (( now >= next_rollout )); then ROLLOUTS=$((ROLLOUTS + 1)); churn_rollout "$ROLLOUTS" & next_rollout=$((next_rollout + ROLLOUT)); fi
  if (( now >= next_kill )); then churn_kill_pod; next_kill=$((next_kill + KILL)); fi
  sleep 2
done
cleanup
