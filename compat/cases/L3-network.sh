# L3: Services, cluster DNS and pod-to-pod networking.

# Deploy 2 nginx replicas behind a ClusterIP Service named `web`.
web_service() {
    k -n "$NS" create deployment web --image=nginx:alpine --replicas=2 --port=80 >/dev/null
    k -n "$NS" expose deployment web --port=80 >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=150s >/dev/null
}

case_def svc_clusterip L3 240 "a ClusterIP Service is reachable by IP from a pod"
t_svc_clusterip() {
    use_ns
    web_service
    client_pod
    local ip
    ip=$(k -n "$NS" get svc web -o jsonpath='{.spec.clusterIP}')
    wait_for 30 kubectl -n "$NS" exec client -- wget -qO- -T 3 "http://$ip/"
}

case_def svc_dns L3 240 "a Service resolves by DNS name (short and FQDN)"
t_svc_dns() {
    use_ns
    web_service
    client_pod
    wait_for 30 kubectl -n "$NS" exec client -- wget -qO- -T 3 http://web/
    wait_for 30 kubectl -n "$NS" exec client -- wget -qO- -T 3 "http://web.$NS.svc.cluster.local/"
}

case_def svc_endpoints L3 240 "Endpoints/EndpointSlices list the ready pod IPs"
t_svc_endpoints() {
    use_ns
    web_service
    wait_for 30 bash -c "[[ \$(kubectl -n $NS get endpointslices -l kubernetes.io/service-name=web -o jsonpath='{.items[*].endpoints[*].addresses[*]}' | wc -w | tr -d ' ') == 2 ]]"
}

case_def svc_headless L3 240 "a headless Service resolves to the pod IPs"
t_svc_headless() {
    use_ns
    k -n "$NS" create deployment web --image=nginx:alpine --replicas=2 >/dev/null
    k -n "$NS" create service clusterip web --clusterip=None --tcp=80:80 >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=150s >/dev/null
    client_pod
    wait_for 30 bash -c "[[ \$(kubectl -n $NS exec client -- nslookup web.$NS.svc.cluster.local 2>/dev/null | grep -c '^Address: ') -ge 2 ]]"
}

case_def svc_nodeport L3 240 "a NodePort Service answers on a node IP"
t_svc_nodeport() {
    use_ns
    web_service
    k -n "$NS" patch svc web -p '{"spec":{"type":"NodePort"}}' >/dev/null
    client_pod
    local port node_ip
    port=$(k -n "$NS" get svc web -o jsonpath='{.spec.ports[0].nodePort}')
    node_ip=$(k get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')
    wait_for 30 kubectl -n "$NS" exec client -- wget -qO- -T 3 "http://$node_ip:$port/"
}

case_def pod_to_pod L3 240 "pods reach each other by pod IP (across nodes when there are several)"
t_pod_to_pod() {
    use_ns
    web_service
    client_pod
    for ip in $(k -n "$NS" get pods -l app=web -o jsonpath='{.items[*].status.podIP}'); do
        wait_for 30 kubectl -n "$NS" exec client -- wget -qO- -T 3 "http://$ip/"
    done
}

case_def api_from_pod L3 180 "a pod reaches the API via the kubernetes Service and its SA token"
t_api_from_pod() {
    use_ns
    client_pod
    wait_for 30 kubectl -n "$NS" exec client -- sh -c \
        'wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version'
}
