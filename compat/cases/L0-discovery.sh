# L0: the API is reachable and discoverable. kubectl, Helm and every controller
# start here; nothing else works without it.

case_def version L0 30 "kubectl version reports a server version"
t_version() {
    out=$(k version -o json)
    expect_contains "$out" '"serverVersion"'
    expect_contains "$out" '"gitVersion"'
}

case_def healthz L0 30 "/healthz, /livez and /readyz answer ok"
t_healthz() {
    expect_eq "$(k get --raw /healthz)" ok
    expect_eq "$(k get --raw /livez)" ok
    expect_eq "$(k get --raw /readyz)" ok
}

case_def api_versions L0 30 "kubectl api-versions lists v1"
t_api_versions() {
    out=$(k api-versions)
    grep -qx v1 <<<"$out"
}

case_def api_resources_core L0 30 "kubectl api-resources lists the core config resources"
t_api_resources_core() {
    out=$(k api-resources --api-group='' -o name)
    for r in namespaces configmaps secrets serviceaccounts pods services; do
        grep -qx "$r" <<<"$out" || { echo "missing resource $r" >&2; return 1; }
    done
}

case_def api_resources_apps L0 30 "kubectl api-resources lists apps/v1 workloads"
t_api_resources_apps() {
    out=$(k api-resources --api-group=apps -o name)
    for r in deployments.apps replicasets.apps statefulsets.apps daemonsets.apps; do
        grep -qx "$r" <<<"$out" || { echo "missing resource $r" >&2; return 1; }
    done
}

case_def default_namespaces L0 30 "default, kube-system and kube-public exist"
t_default_namespaces() {
    k get namespace default kube-system kube-public >/dev/null
}

case_def openapi_v3 L0 30 "/openapi/v3 serves a discovery document"
t_openapi_v3() {
    out=$(k get --raw /openapi/v3)
    expect_contains "$out" '"paths"'
}
