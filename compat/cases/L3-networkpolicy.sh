# L3: NetworkPolicy enforcement (ingress, egress, selectors, ipBlock, DNS).

# nginx (app=web, container port named `http`) behind a Service `web`, in
# namespace $1.
np_web() {
    k -n "$1" apply -f - >/dev/null <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: 1
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
        - name: nginx
          image: nginx:alpine
          ports: [{name: http, containerPort: 80}]
---
apiVersion: v1
kind: Service
metadata: {name: web}
spec:
  selector: {app: web}
  ports: [{port: 80, targetPort: http}]
EOF
    k -n "$1" rollout status deployment/web --timeout=150s >/dev/null
}

# np_client <namespace> <name> <labels>: a busybox pod, Ready.
np_client() {
    k -n "$1" run "$2" --image=busybox:1.36 --restart=Never --labels="$3" -- sleep 3600 >/dev/null
    k -n "$1" wait --for=condition=Ready "pod/$2" --timeout=120s >/dev/null
}

# np_get <namespace> <pod> <url>: one short HTTP request from the pod.
np_get() {
    kubectl -n "$1" exec "$2" -- wget -qO- -T 2 "$3"
}

# Wait until `"$@"` fails three times in a row (a policy took effect).
np_denied() {
    local secs=$1
    shift
    local deadline=$((SECONDS + secs)) fails=0
    while ((SECONDS < deadline)); do
        if "$@" >/dev/null 2>&1; then
            fails=0
        else
            fails=$((fails + 1))
            ((fails >= 3)) && return 0
        fi
        sleep 1
    done
    echo "still allowed after ${secs}s: $*" >&2
    return 1
}

np_web_ip() {
    k -n "$1" get pods -l app=web -o jsonpath='{.items[0].status.podIP}'
}

case_def netpol_ingress_pods L3 300 "a NetworkPolicy admits only the selected client pods, on a named port"
t_netpol_ingress_pods() {
    use_ns
    np_web "$NS"
    np_client "$NS" allowed role=client
    np_client "$NS" other role=other
    local ip
    ip=$(np_web_ip "$NS")
    wait_for 30 np_get "$NS" other "http://$ip/"
    k -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: web}
spec:
  podSelector: {matchLabels: {app: web}}
  policyTypes: [Ingress]
  ingress:
    - from: [{podSelector: {matchLabels: {role: client}}}]
      ports: [{port: http, protocol: TCP}]
EOF
    np_denied 60 np_get "$NS" other "http://$ip/"
    np_denied 30 np_get "$NS" other http://web/
    wait_for 30 np_get "$NS" allowed "http://$ip/"
    wait_for 30 np_get "$NS" allowed http://web/
    # Gone with the policy.
    k -n "$NS" delete networkpolicy web >/dev/null
    wait_for 60 np_get "$NS" other http://web/
}

case_def netpol_namespaces L3 300 "default deny, then namespaceSelector alone and combined with podSelector"
t_netpol_namespaces() {
    use_ns
    local peer="$NS-peer"
    k create namespace "$peer" >/dev/null 2>&1 || k get namespace "$peer" >/dev/null
    # Expanded now: $peer is local and gone when the subshell exits.
    trap "kubectl delete namespace '$peer' --wait=false >/dev/null 2>&1" EXIT
    np_web "$NS"
    np_client "$NS" local role=client
    np_client "$peer" client role=client
    np_client "$peer" other role=other
    k -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: default-deny}
spec:
  podSelector: {}
  policyTypes: [Ingress]
EOF
    np_denied 60 np_get "$NS" local "http://web/"
    np_denied 30 np_get "$peer" client "http://web.$NS/"
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: from-peer}
spec:
  podSelector: {matchLabels: {app: web}}
  ingress:
    - from: [{namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: $peer}}}]
EOF
    wait_for 60 np_get "$peer" client "http://web.$NS/"
    wait_for 30 np_get "$peer" other "http://web.$NS/"
    np_denied 30 np_get "$NS" local "http://web/"
    # Both selectors in one peer: that namespace's clients only.
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: from-peer}
spec:
  podSelector: {matchLabels: {app: web}}
  ingress:
    - from:
        - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: $peer}}
          podSelector: {matchLabels: {role: client}}
EOF
    np_denied 60 np_get "$peer" other "http://web.$NS/"
    wait_for 30 np_get "$peer" client "http://web.$NS/"
    np_denied 30 np_get "$NS" local "http://web/"
}

case_def netpol_egress_dns L3 300 "egress isolation: DNS and the allowed pods only, and no DNS unless allowed"
t_netpol_egress_dns() {
    use_ns
    np_web "$NS"
    np_client "$NS" client role=client
    local ip
    ip=$(np_web_ip "$NS")
    wait_for 30 np_get "$NS" client http://web/
    k -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: client}
spec:
  podSelector: {matchLabels: {role: client}}
  policyTypes: [Egress]
  egress:
    - to:
        - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: kube-system}}
          podSelector: {matchLabels: {k8s-app: kube-dns}}
      ports: [{port: 53, protocol: UDP}, {port: 53, protocol: TCP}]
    - to: [{podSelector: {matchLabels: {app: web}}}]
      ports: [{port: 80}]
EOF
    # The API server is not among the allowed destinations.
    np_denied 60 kubectl -n "$NS" exec client -- wget -qO- -T 2 --no-check-certificate https://kubernetes.default.svc/version
    wait_for 30 np_get "$NS" client http://web/
    # Without the DNS rule, names no longer resolve; the web pod still answers by address.
    k -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: client}
spec:
  podSelector: {matchLabels: {role: client}}
  policyTypes: [Egress]
  egress:
    - to: [{podSelector: {matchLabels: {app: web}}}]
      ports: [{port: 80}]
EOF
    np_denied 60 kubectl -n "$NS" exec client -- nslookup -timeout=2 web
    wait_for 30 np_get "$NS" client "http://$ip/"
}

case_def netpol_ipblock L3 300 "an ipBlock with except admits every address but the excepted one"
t_netpol_ipblock() {
    use_ns
    np_web "$NS"
    np_client "$NS" a role=client
    np_client "$NS" b role=client
    local ip a_ip
    ip=$(np_web_ip "$NS")
    a_ip=$(k -n "$NS" get pod a -o jsonpath='{.status.podIP}')
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: web}
spec:
  podSelector: {matchLabels: {app: web}}
  ingress:
    - from: [{ipBlock: {cidr: 0.0.0.0/0, except: ["$a_ip/32"]}}]
      ports: [{port: 80, endPort: 90}]
EOF
    np_denied 60 np_get "$NS" a "http://$ip/"
    wait_for 30 np_get "$NS" b "http://$ip/"
}
