# M2: litek8s's overlay relay (docs/networking.md). Not part of the default
# tiers and with no k3s equivalent (flannel has no relay); run with
# `--tiers M2` against a litek8s cluster of a server and two or more agents.
#
# The cases cut the network path between two worker nodes and restore it
# through commands the caller provides (the two node names are appended):
#   COMPAT_PATH_BLOCK    e.g. "bench/nat-e2e.sh block"
#   COMPAT_PATH_UNBLOCK  e.g. "bench/nat-e2e.sh unblock"
# Uses node_names, need_nodes and pinned_server from M1-multinode.sh when
# both tiers run; defined again here so M2 runs alone.

m2_workers() {
    k get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'
}

m2_need() {
    if [[ -z "${COMPAT_PATH_BLOCK:-}" || -z "${COMPAT_PATH_UNBLOCK:-}" ]]; then
        echo "set COMPAT_PATH_BLOCK and COMPAT_PATH_UNBLOCK to cut the path between two nodes" >&2
        return 1
    fi
    if (($(m2_workers | wc -l) < 2)); then
        echo "needs two worker nodes" >&2
        return 1
    fi
}

m2_server() { # name node
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $1, labels: {app: who, at: $2}}
spec:
  nodeSelector: {kubernetes.io/hostname: $2}
  containers:
  - name: httpd
    image: busybox:1.36
    command: [sh, -c, 'mkdir -p /www && hostname > /www/index.html && exec httpd -f -vv -p 8080 -h /www']
EOF
}

# The route node <a> has to node <b> (direct, "via <relay>", unreachable).
m2_route() { # a b
    kubectl get node "$1" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}' |
        python3 -c 'import json, sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$2"
}
export -f m2_route

m2_get() { # from-pod url want
    [[ "$(kubectl -n "$NS" exec "$1" -- wget -qO- -T 3 "$2")" == "$3" ]]
}
export -f m2_get

case_def relay_no_direct_path M2 600 "two nodes with no direct path reach each other's pods and Services through a relay, and go back to direct when the path returns"
t_relay_no_direct_path() {
    m2_need
    use_ns
    local a b ip_a ip_b
    a=$(m2_workers | sed -n 1p)
    b=$(m2_workers | sed -n 2p)
    m2_server pa "$a"
    m2_server pb "$b"
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Service
metadata: {name: who-b}
spec:
  selector: {at: $b}
  ports: [{port: 80, targetPort: 8080}]
EOF
    k -n "$NS" wait --for=condition=Ready pod/pa pod/pb --timeout=150s >/dev/null
    ip_a=$(k -n "$NS" get pod pa -o jsonpath='{.status.podIP}')
    ip_b=$(k -n "$NS" get pod pb -o jsonpath='{.status.podIP}')
    wait_for 60 m2_get pa "http://$ip_b:8080/" pb
    trap '$COMPAT_PATH_UNBLOCK '"$a $b"' >/dev/null 2>&1 || true' EXIT
    $COMPAT_PATH_BLOCK "$a" "$b" >/dev/null
    # Relayed: by a node that is neither of them, the same seen from both.
    wait_for 240 bash -c "[[ \$(m2_route $a $b) == via* ]]"
    wait_for 60 bash -c "[[ \$(m2_route $b $a) == \$(m2_route $a $b) ]]"
    local relay
    relay=$(m2_route "$a" "$b")
    relay=${relay#via }
    [[ "$relay" != "$a" && "$relay" != "$b" ]] || { echo "relayed by $relay, one of the pair" >&2; return 1; }
    wait_for 120 m2_get pa "http://$ip_b:8080/" pb
    expect_eq "$(k -n "$NS" exec pb -- wget -qO- -T 3 "http://$ip_a:8080/")" "pa"
    wait_for 30 m2_get pa "http://who-b.$NS.svc.cluster.local/" pb
    # Still the pod address at the far end, not the relay's.
    wait_for 10 bash -c "kubectl -n $NS logs pb | grep -q '$ip_a'"
    $COMPAT_PATH_UNBLOCK "$a" "$b" >/dev/null
    trap - EXIT
    wait_for 300 bash -c "[[ \$(m2_route $a $b) == direct && \$(m2_route $b $a) == direct ]]"
    wait_for 60 m2_get pa "http://$ip_b:8080/" pb
}

case_def relay_routes_annotated M2 60 "every overlay node carries its observations and routes, and relays are servers or labeled ones"
t_relay_routes_annotated() {
    local n json
    for n in $(k get nodes -o jsonpath='{.items[*].metadata.name}'); do
        json=$(k get node "$n" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}')
        [[ -n "$json" ]] || { echo "node $n has no routes annotation" >&2; return 1; }
        python3 -c 'import json, sys; r = json.loads(sys.argv[1]); assert all(v == "direct" or v == "unreachable" or v.startswith("via ") for v in r.values()), r' "$json"
        [[ -n "$(k get node "$n" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-peers}')" ]] || { echo "node $n has no peers annotation" >&2; return 1; }
    done
}
