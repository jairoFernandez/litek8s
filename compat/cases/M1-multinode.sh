# M1: multi-node behaviour. Not part of the default tiers (they run on one
# node too); run with `--tiers M1` against a cluster of two or more nodes.
#
# The node-failure cases power a worker node off and on again through
# commands the caller provides (the node name is appended):
#   COMPAT_NODE_KILL   e.g. "bench/vm.sh kill"   or "docker kill"  (k3d)
#   COMPAT_NODE_START  e.g. "bench/vm.sh start"  or "docker start" (k3d)
# They never touch a node labelled node-role.kubernetes.io/control-plane.

# Names of the nodes, one per line.
node_names() {
    k get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'
}

need_nodes() { # count
    local n
    n=$(node_count)
    if ((n < $1)); then
        echo "needs at least $1 nodes, the cluster has $n" >&2
        return 1
    fi
}

# A worker: the first node without the control-plane role.
worker_node() {
    k get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[0].metadata.name}'
}

need_power_commands() {
    if [[ -z "${COMPAT_NODE_KILL:-}" || -z "${COMPAT_NODE_START:-}" ]]; then
        echo "set COMPAT_NODE_KILL and COMPAT_NODE_START to power nodes off and on" >&2
        return 1
    fi
}

node_ready() { # node
    [[ "$(kubectl get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" == True ]]
}

node_not_ready() { # node
    [[ "$(kubectl get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" != True ]]
}

# A busybox pod pinned to a node that serves its own name on :8080.
pinned_server() { # name node
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $1, labels: {app: who}}
spec:
  nodeSelector: {kubernetes.io/hostname: $2}
  containers:
  - name: httpd
    image: busybox:1.36
    command: [sh, -c, 'mkdir -p /www && hostname > /www/index.html && exec httpd -f -vv -p 8080 -h /www']
    ports: [{containerPort: 8080}]
EOF
}

case_def multi_pod_to_pod M1 240 "pods on different nodes reach each other by pod IP, source addresses kept"
t_multi_pod_to_pod() {
    need_nodes 2
    use_ns
    local nodes a b ip_a ip_b
    nodes=$(node_names)
    a=$(echo "$nodes" | sed -n 1p)
    b=$(echo "$nodes" | sed -n 2p)
    pinned_server pa "$a"
    pinned_server pb "$b"
    k -n "$NS" wait --for=condition=Ready pod/pa pod/pb --timeout=150s >/dev/null
    ip_a=$(k -n "$NS" get pod pa -o jsonpath='{.status.podIP}')
    ip_b=$(k -n "$NS" get pod pb -o jsonpath='{.status.podIP}')
    [[ "$ip_a" != "$ip_b" ]] || { echo "both pods got $ip_a" >&2; return 1; }
    wait_for 30 kubectl -n "$NS" exec pa -- wget -qO- -T 3 "http://$ip_b:8080/"
    expect_eq "$(k -n "$NS" exec pa -- wget -qO- -T 3 "http://$ip_b:8080/")" "pb"
    expect_eq "$(k -n "$NS" exec pb -- wget -qO- -T 3 "http://$ip_a:8080/")" "pa"
    # The server sees the client's pod address, not its node's.
    wait_for 10 bash -c "kubectl -n $NS logs pb | grep -q '$ip_a'"
}

case_def multi_daemonset_every_node M1 240 "a DaemonSet runs on every node and every node's pod is reachable, by IP and through DNS"
t_multi_daemonset_every_node() {
    need_nodes 2
    use_ns
    k -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: who}
spec:
  selector: {matchLabels: {app: who}}
  template:
    metadata: {labels: {app: who}}
    spec:
      containers:
      - name: httpd
        image: busybox:1.36
        command: [sh, -c, 'mkdir -p /www && hostname > /www/index.html && exec httpd -f -p 8080 -h /www']
        ports: [{containerPort: 8080}]
---
apiVersion: v1
kind: Service
metadata: {name: who}
spec:
  selector: {app: who}
  ports: [{port: 80, targetPort: 8080}]
EOF
    k -n "$NS" rollout status daemonset/who --timeout=180s >/dev/null
    local n pods nodes_with
    n=$(node_count)
    expect_eq "$(k -n "$NS" get ds who -o jsonpath='{.status.numberReady}')" "$n"
    nodes_with=$(k -n "$NS" get pods -l app=who -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u | wc -l | tr -d ' ')
    expect_eq "$nodes_with" "$n"
    pods=$(k -n "$NS" get pods -l app=who -o jsonpath='{.items[*].metadata.name}')
    # Every pod reaches every other pod, the Service and the API by name.
    for from in $pods; do
        for ip in $(k -n "$NS" get pods -l app=who -o jsonpath='{.items[*].status.podIP}'); do
            wait_for 20 kubectl -n "$NS" exec "$from" -- wget -qO- -T 3 "http://$ip:8080/"
        done
        wait_for 20 kubectl -n "$NS" exec "$from" -- wget -qO- -T 3 "http://who.$NS.svc.cluster.local/"
        wait_for 20 kubectl -n "$NS" exec "$from" -- nslookup kubernetes.default.svc.cluster.local
    done
}

case_def multi_service_spreads M1 240 "a Service balances over endpoints on several nodes"
t_multi_service_spreads() {
    need_nodes 2
    use_ns
    local nodes a b
    nodes=$(node_names)
    a=$(echo "$nodes" | sed -n 1p)
    b=$(echo "$nodes" | sed -n 2p)
    pinned_server pa "$a"
    pinned_server pb "$b"
    k -n "$NS" create service clusterip who --tcp=80:8080 >/dev/null
    k -n "$NS" wait --for=condition=Ready pod/pa pod/pb --timeout=150s >/dev/null
    wait_for 30 bash -c "[[ \$(kubectl -n $NS get endpoints who -o jsonpath='{.subsets[*].addresses[*].ip}' | wc -w | tr -d ' ') == 2 ]]"
    # From a pod on the first node, the Service must answer from both (the
    # proxy may take a moment to program the second endpoint).
    wait_for 60 bash -c "kubectl -n $NS exec pa -- sh -c 'for i in \$(seq 1 20); do wget -qO- -T 3 http://who/; done' | sort -u | tr '\\n' ' ' | grep -q 'pa pb'"
}

# Running, not deleting pods of app=web in <ns> on nodes other than <node>.
running_elsewhere() { # ns node
    kubectl -n "$1" get pods -l app=web -o json | python3 -c '
import json, sys
node = sys.argv[1]
print(sum(1 for p in json.load(sys.stdin)["items"]
          if p["spec"].get("nodeName") != node and p["status"].get("phase") == "Running"
          and not p["metadata"].get("deletionTimestamp")))' "$2"
}
export -f running_elsewhere

case_def multi_node_kill_deployment M1 600 "a Deployment keeps its replicas when a worker node dies, and the node rejoins cleanly"
t_multi_node_kill_deployment() {
    need_nodes 2
    need_power_commands
    use_ns
    local worker replicas=4
    worker=$(worker_node)
    [[ -n "$worker" ]] || { echo "no worker node (every node is a control plane)" >&2; return 1; }
    # Short tolerations, so the test does not wait the default 300 s.
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: $replicas
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      tolerations:
      - {key: node.kubernetes.io/unreachable, operator: Exists, effect: NoExecute, tolerationSeconds: 10}
      - {key: node.kubernetes.io/not-ready, operator: Exists, effect: NoExecute, tolerationSeconds: 10}
      containers:
      - {name: web, image: nginx:alpine, ports: [{containerPort: 80}]}
EOF
    k -n "$NS" rollout status deployment/web --timeout=150s >/dev/null
    # Make sure some replica runs on the worker: pin one there if needed.
    if ! k -n "$NS" get pods -l app=web -o jsonpath='{.items[*].spec.nodeName}' | tr ' ' '\n' | grep -qx "$worker"; then
        echo "no replica landed on $worker" >&2
        k -n "$NS" get pods -o wide >&2
        return 1
    fi
    trap '$COMPAT_NODE_START '"$worker"' >/dev/null 2>&1 || true' EXIT
    $COMPAT_NODE_KILL "$worker" >/dev/null
    wait_for 150 node_not_ready "$worker"
    # Replacements run elsewhere; the old pods may linger as Terminating.
    wait_for 180 bash -c "[[ \$(running_elsewhere $NS $worker) -ge $replicas ]]"
    $COMPAT_NODE_START "$worker" >/dev/null
    trap - EXIT
    wait_for 300 node_ready "$worker"
    # After the rejoin exactly the desired replicas are left, all running.
    wait_for 180 bash -c "[[ \$(kubectl -n $NS get pods -l app=web --no-headers | wc -l | tr -d ' ') == $replicas && \$(kubectl -n $NS get pods -l app=web --no-headers | grep -c Running) == $replicas ]]"
    expect_eq "$(k -n "$NS" get deployment web -o jsonpath='{.status.availableReplicas}')" "$replicas"
}

case_def multi_node_short_outage M1 600 "a node that is back within its pods' tolerations keeps them"
t_multi_node_short_outage() {
    need_nodes 2
    need_power_commands
    use_ns
    local worker before after
    worker=$(worker_node)
    [[ -n "$worker" ]] || { echo "no worker node (every node is a control plane)" >&2; return 1; }
    pinned_server stay "$worker"
    k -n "$NS" wait --for=condition=Ready pod/stay --timeout=150s >/dev/null
    before=$(k -n "$NS" get pod stay -o jsonpath='{.metadata.uid}')
    trap '$COMPAT_NODE_START '"$worker"' >/dev/null 2>&1 || true' EXIT
    $COMPAT_NODE_KILL "$worker" >/dev/null
    wait_for 150 node_not_ready "$worker"
    $COMPAT_NODE_START "$worker" >/dev/null
    trap - EXIT
    wait_for 300 node_ready "$worker"
    # The same pod (default 300 s tolerations), running again.
    wait_for 120 kubectl -n "$NS" wait --for=condition=Ready pod/stay --timeout=5s
    after=$(k -n "$NS" get pod stay -o jsonpath='{.metadata.uid}')
    expect_eq "$after" "$before"
}
