# L1: generic object API semantics on config objects (no containers needed).
# This is what kubectl apply/patch/watch and Helm's release bookkeeping rely on.

case_def ns_lifecycle L1 60 "create, get and delete a namespace"
t_ns_lifecycle() {
    local ns="compat-ns-life-$RANDOM"
    k create namespace "$ns" >/dev/null
    expect_eq "$(k get namespace "$ns" -o jsonpath='{.status.phase}')" Active
    k delete namespace "$ns" --wait=true --timeout=50s >/dev/null
    ! k get namespace "$ns" >/dev/null 2>&1
}

case_def cm_create_get L1 30 "create a ConfigMap from literals and read it back"
t_cm_create_get() {
    use_ns
    k -n "$NS" create configmap app --from-literal=color=blue --from-literal=size=3 >/dev/null
    expect_eq "$(k -n "$NS" get cm app -o jsonpath='{.data.color}')" blue
    expect_eq "$(k -n "$NS" get cm app -o jsonpath='{.data.size}')" 3
}

case_def cm_metadata L1 30 "server fills uid, resourceVersion and creationTimestamp"
t_cm_metadata() {
    use_ns
    k -n "$NS" create configmap meta --from-literal=a=1 >/dev/null
    [[ -n "$(k -n "$NS" get cm meta -o jsonpath='{.metadata.uid}')" ]]
    [[ -n "$(k -n "$NS" get cm meta -o jsonpath='{.metadata.resourceVersion}')" ]]
    [[ "$(k -n "$NS" get cm meta -o jsonpath='{.metadata.creationTimestamp}')" == *T*Z ]]
}

case_def apply_idempotent L1 30 "kubectl apply twice: created, then unchanged"
t_apply_idempotent() {
    use_ns
    local f
    f=$(mktemp)
    cat >"$f" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: applied
  namespace: $NS
data:
  key: one
EOF
    expect_contains "$(k apply -f "$f")" created
    expect_contains "$(k apply -f "$f")" unchanged
    rm -f "$f"
}

case_def apply_update L1 30 "kubectl apply with a changed field reports configured and updates"
t_apply_update() {
    use_ns
    local f
    f=$(mktemp)
    printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: applied\n  namespace: %s\ndata:\n  key: one\n  drop: me\n' "$NS" >"$f"
    k apply -f "$f" >/dev/null
    printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: applied\n  namespace: %s\ndata:\n  key: two\n' "$NS" >"$f"
    expect_contains "$(k apply -f "$f")" configured
    expect_eq "$(k -n "$NS" get cm applied -o jsonpath='{.data.key}')" two
    # Client-side apply must prune keys removed from the manifest.
    expect_eq "$(k -n "$NS" get cm applied -o jsonpath='{.data.drop}')" ""
    rm -f "$f"
}

case_def apply_server_side L1 30 "kubectl apply --server-side creates and updates"
t_apply_server_side() {
    use_ns
    local f
    f=$(mktemp)
    printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ssa\n  namespace: %s\ndata:\n  key: one\n' "$NS" >"$f"
    expect_contains "$(k apply --server-side -f "$f")" serverside-applied
    printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ssa\n  namespace: %s\ndata:\n  key: two\n' "$NS" >"$f"
    k apply --server-side -f "$f" >/dev/null
    expect_eq "$(k -n "$NS" get cm ssa -o jsonpath='{.data.key}')" two
    rm -f "$f"
}

case_def patch_merge L1 30 "kubectl patch --type merge"
t_patch_merge() {
    use_ns
    k -n "$NS" create configmap p --from-literal=a=1 >/dev/null
    k -n "$NS" patch cm p --type merge -p '{"data":{"b":"2"}}' >/dev/null
    expect_eq "$(k -n "$NS" get cm p -o jsonpath='{.data.a},{.data.b}')" "1,2"
}

case_def patch_json L1 30 "kubectl patch --type json"
t_patch_json() {
    use_ns
    k -n "$NS" create configmap p --from-literal=a=1 >/dev/null
    k -n "$NS" patch cm p --type json -p '[{"op":"replace","path":"/data/a","value":"9"}]' >/dev/null
    expect_eq "$(k -n "$NS" get cm p -o jsonpath='{.data.a}')" 9
}

case_def patch_strategic L1 30 "kubectl patch (strategic merge, the default)"
t_patch_strategic() {
    use_ns
    k -n "$NS" create configmap p --from-literal=a=1 >/dev/null
    k -n "$NS" patch cm p -p '{"data":{"a":null,"c":"3"}}' >/dev/null
    expect_eq "$(k -n "$NS" get cm p -o jsonpath='{.data.a}|{.data.c}')" "|3"
}

case_def label_annotate L1 30 "kubectl label and annotate, then select by label"
t_label_annotate() {
    use_ns
    k -n "$NS" create configmap one --from-literal=a=1 >/dev/null
    k -n "$NS" create configmap two --from-literal=a=1 >/dev/null
    k -n "$NS" label cm one tier=front >/dev/null
    k -n "$NS" label cm two tier=back >/dev/null
    k -n "$NS" annotate cm one owner=compat >/dev/null
    expect_eq "$(k -n "$NS" get cm -l tier=front -o jsonpath='{.items[*].metadata.name}')" one
    expect_eq "$(k -n "$NS" get cm -l 'tier,tier!=front' -o jsonpath='{.items[*].metadata.name}')" two
    expect_eq "$(k -n "$NS" get cm one -o jsonpath='{.metadata.annotations.owner}')" compat
}

case_def field_selector L1 30 "list with --field-selector metadata.name"
t_field_selector() {
    use_ns
    k -n "$NS" create configmap a1 --from-literal=a=1 >/dev/null
    k -n "$NS" create configmap a2 --from-literal=a=1 >/dev/null
    expect_eq "$(k -n "$NS" get cm --field-selector metadata.name=a2 -o jsonpath='{.items[*].metadata.name}')" a2
}

case_def secret_generic L1 30 "Secret from literal is stored base64 and decodes back"
t_secret_generic() {
    use_ns
    k -n "$NS" create secret generic s --from-literal=password=hunter2 >/dev/null
    expect_eq "$(k -n "$NS" get secret s -o jsonpath='{.data.password}' | base64 -d)" hunter2
    expect_eq "$(k -n "$NS" get secret s -o jsonpath='{.type}')" Opaque
}

case_def replace_put L1 30 "kubectl replace (PUT) overwrites the object"
t_replace_put() {
    use_ns
    k -n "$NS" create configmap r --from-literal=a=1 >/dev/null
    k -n "$NS" get cm r -o json | sed 's/"a": "1"/"a": "5"/' | k replace -f - >/dev/null
    expect_eq "$(k -n "$NS" get cm r -o jsonpath='{.data.a}')" 5
}

case_def update_conflict L1 30 "PUT with a stale resourceVersion is rejected with Conflict"
t_update_conflict() {
    use_ns
    k -n "$NS" create configmap c --from-literal=a=1 >/dev/null
    local stale
    stale=$(k -n "$NS" get cm c -o json)
    k -n "$NS" patch cm c --type merge -p '{"data":{"a":"2"}}' >/dev/null
    if out=$(sed 's/"a": "1"/"a": "3"/' <<<"$stale" | k replace -f - 2>&1); then
        echo "stale replace unexpectedly succeeded" >&2
        return 1
    fi
    expect_contains "$out" "the object has been modified"
}

case_def generate_name L1 30 "metadata.generateName produces a unique name"
t_generate_name() {
    use_ns
    local out
    out=$(printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  generateName: gen-\n  namespace: %s\n' "$NS" | k create -f - -o name)
    [[ "$out" == configmap/gen-?* ]]
}

case_def already_exists L1 30 "creating a duplicate fails with AlreadyExists"
t_already_exists() {
    use_ns
    k -n "$NS" create configmap dup --from-literal=a=1 >/dev/null
    if out=$(k -n "$NS" create configmap dup --from-literal=a=1 2>&1); then
        echo "duplicate create unexpectedly succeeded" >&2
        return 1
    fi
    expect_contains "$out" "already exists"
}

case_def not_found L1 30 "getting a missing object fails with NotFound"
t_not_found() {
    use_ns
    if out=$(k -n "$NS" get cm missing 2>&1); then
        echo "get of missing object succeeded" >&2
        return 1
    fi
    expect_contains "$out" "not found"
}

case_def watch_events L1 45 "kubectl get -w streams ADDED and MODIFIED events"
t_watch_events() {
    use_ns
    local log
    log=$(mktemp)
    with_timeout 30 kubectl -n "$NS" get cm -w --output-watch-events >"$log" 2>&1 &
    local watcher=$!
    sleep 2
    k -n "$NS" create configmap w --from-literal=a=1 >/dev/null
    k -n "$NS" patch cm w --type merge -p '{"data":{"a":"2"}}' >/dev/null
    wait_for 20 grep -Eq '^MODIFIED +w ' "$log" || { cat "$log" >&2; kill $watcher 2>/dev/null; return 1; }
    grep -Eq '^ADDED +w ' "$log"
    kill $watcher 2>/dev/null || true
    rm -f "$log"
}

case_def watch_from_rv L1 45 "watch from a list resourceVersion only sends newer events"
t_watch_from_rv() {
    use_ns
    k -n "$NS" create configmap old --from-literal=a=1 >/dev/null
    local rv out
    rv=$(k get --raw "/api/v1/namespaces/$NS/configmaps" | sed -n 's/.*"resourceVersion":"\([0-9]*\)".*/\1/p' | head -1)
    [[ -n "$rv" ]]
    (sleep 2; k -n "$NS" create configmap new --from-literal=a=1 >/dev/null) &
    out=$(k get --raw "/api/v1/namespaces/$NS/configmaps?watch=true&resourceVersion=$rv&timeoutSeconds=6" || true)
    wait
    expect_contains "$out" '"name":"new"'
    if [[ "$out" == *'"name":"old"'* ]]; then
        echo "watch replayed an object older than the resourceVersion" >&2
        return 1
    fi
}

case_def ns_cascade L1 60 "deleting a namespace deletes its objects"
t_ns_cascade() {
    local ns="compat-ns-cascade-$RANDOM"
    k create namespace "$ns" >/dev/null
    k -n "$ns" create configmap inside --from-literal=a=1 >/dev/null
    k delete namespace "$ns" --wait=true --timeout=50s >/dev/null
    ! k -n "$ns" get cm inside >/dev/null 2>&1
}

case_def dry_run_server L1 30 "--dry-run=server validates without persisting"
t_dry_run_server() {
    use_ns
    k -n "$NS" create configmap dry --from-literal=a=1 --dry-run=server >/dev/null
    ! k -n "$NS" get cm dry >/dev/null 2>&1
}

case_def list_all_namespaces L1 30 "kubectl get -A lists objects across namespaces"
t_list_all_namespaces() {
    use_ns
    k -n "$NS" create configmap everywhere --from-literal=a=1 >/dev/null
    out=$(k get cm -A -o jsonpath='{range .items[*]}{.metadata.namespace}/{.metadata.name}{"\n"}{end}')
    grep -qx "$NS/everywhere" <<<"$out"
}

case_def table_output L1 30 "kubectl get prints NAME/DATA/AGE columns"
t_table_output() {
    use_ns
    k -n "$NS" create configmap tbl --from-literal=a=1 >/dev/null
    local out
    out=$(k -n "$NS" get cm tbl)
    expect_contains "$out" NAME
    expect_contains "$out" DATA
    expect_contains "$out" AGE
}

case_def crd_roundtrip L1 60 "register a CRD and create a custom object"
t_crd_roundtrip() {
    use_ns
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: widgets.compat.litek8s.dev
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: widgets, singular: widget, kind: Widget}
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          x-kubernetes-preserve-unknown-fields: true
EOF
    wait_for 30 k get --raw /apis/compat.litek8s.dev/v1
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: Widget\nmetadata:\n  name: w1\n  namespace: %s\nspec:\n  size: 3\n' "$NS" | k apply -f - >/dev/null
    expect_eq "$(k -n "$NS" get widget w1 -o jsonpath='{.spec.size}')" 3
    k delete crd widgets.compat.litek8s.dev --wait=false >/dev/null
}

case_def serviceaccount_default L1 30 "every namespace gets a default ServiceAccount"
t_serviceaccount_default() {
    use_ns
    wait_for 20 k -n "$NS" get serviceaccount default
}

# Create the manifest on stdin in $NS; it must be rejected as Invalid with
# every given message.
expect_invalid() {
    local manifest out msg
    manifest=$(cat)
    if out=$(printf '%s\n' "$manifest" | k -n "$NS" create -f - 2>&1); then
        echo "invalid object was accepted: $out" >&2
        return 1
    fi
    expect_contains "$out" "is invalid" || return 1
    for msg in "$@"; do
        expect_contains "$out" "$msg" || return 1
    done
}

case_def invalid_names L1 30 "object names follow the kind's DNS rule"
t_invalid_names() {
    use_ns
    expect_invalid 'metadata.name: Invalid value: "Bad_Name": a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters' <<'EOF2'
apiVersion: v1
kind: ConfigMap
metadata: {name: Bad_Name}
EOF2
    expect_invalid 'metadata.name: Invalid value: "1web": a DNS-1035 label must consist of lower case alphanumeric characters' <<'EOF2'
apiVersion: v1
kind: Service
metadata: {name: 1web}
spec: {ports: [{port: 80}]}
EOF2
    expect_invalid 'metadata.name: Invalid value: "a.b": must not contain dots' <<'EOF2'
apiVersion: v1
kind: Namespace
metadata: {name: a.b}
EOF2
}

case_def invalid_labels L1 30 "label keys and values are validated"
t_invalid_labels() {
    use_ns
    expect_invalid 'metadata.labels: Invalid value: "bad key": name part must consist of alphanumeric characters' \
        'metadata.labels: Invalid value: "-x": a valid label must be an empty string or consist of alphanumeric characters' <<'EOF2'
apiVersion: v1
kind: ConfigMap
metadata:
  name: labels
  labels: {"bad key": ok, good: "-x"}
EOF2
    expect_invalid 'metadata.annotations: Invalid value: "a/b/c": a qualified name must consist of' <<'EOF2'
apiVersion: v1
kind: ConfigMap
metadata:
  name: annotations
  annotations: {"a/b/c": x}
EOF2
}

case_def invalid_workload_selector L1 30 "workloads need a selector matching their template"
t_invalid_workload_selector() {
    use_ns
    expect_invalid 'spec.selector: Required value' \
        'spec.template.metadata.labels: Invalid value: map[string]string{"app":"web"}: `selector` does not match template `labels`' <<'EOF2'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  template:
    metadata: {labels: {app: web}}
    spec: {containers: [{name: web, image: nginx}]}
EOF2
    expect_invalid '`selector` does not match template `labels`' <<'EOF2'
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: db}
spec:
  selector: {matchLabels: {app: db}}
  template:
    metadata: {labels: {app: other}}
    spec: {containers: [{name: db, image: nginx}]}
EOF2
    expect_invalid 'spec.template.spec.restartPolicy: Unsupported value: "Never": supported values: "Always"' <<'EOF2'
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: agent}
spec:
  selector: {matchLabels: {app: agent}}
  template:
    metadata: {labels: {app: agent}}
    spec: {restartPolicy: Never, containers: [{name: agent, image: nginx}]}
EOF2
}

case_def invalid_pod_containers L1 30 "pods need containers with unique names and valid ports"
t_invalid_pod_containers() {
    use_ns
    expect_invalid 'spec.containers: Required value' <<'EOF2'
apiVersion: v1
kind: Pod
metadata: {name: empty}
spec: {containers: []}
EOF2
    expect_invalid 'spec.containers[1].name: Duplicate value: "a"' \
        'spec.containers[0].ports[0].containerPort: Invalid value: 70000: must be between 1 and 65535, inclusive' \
        'spec.containers[1].image: Required value' <<'EOF2'
apiVersion: v1
kind: Pod
metadata: {name: dup}
spec:
  containers:
    - {name: a, image: nginx, ports: [{containerPort: 70000}]}
    - {name: a}
EOF2
}

case_def invalid_service_ports L1 30 "service ports must be in range with a known protocol"
t_invalid_service_ports() {
    use_ns
    expect_invalid 'spec.ports[0].port: Invalid value: 70000: must be between 1 and 65535, inclusive' <<'EOF2'
apiVersion: v1
kind: Service
metadata: {name: big}
spec: {ports: [{port: 70000, targetPort: 80}]}
EOF2
    expect_invalid 'spec.ports[0].protocol: Unsupported value: "HTTP": supported values: "SCTP", "TCP", "UDP"' \
        'spec.ports[1].name: Required value' <<'EOF2'
apiVersion: v1
kind: Service
metadata: {name: proto}
spec: {ports: [{name: a, port: 80, protocol: HTTP}, {port: 81}]}
EOF2
}

case_def invalid_job_restart L1 30 "job pods must restart OnFailure or Never"
t_invalid_job_restart() {
    use_ns
    expect_invalid 'spec.template.spec.restartPolicy: Unsupported value: "Always": supported values: "OnFailure", "Never"' <<'EOF2'
apiVersion: batch/v1
kind: Job
metadata: {name: always}
spec:
  template:
    spec: {restartPolicy: Always, containers: [{name: a, image: busybox}]}
EOF2
    expect_invalid 'spec.template.spec.restartPolicy: Required value: valid values: "OnFailure", "Never"' <<'EOF2'
apiVersion: batch/v1
kind: Job
metadata: {name: unset}
spec:
  template:
    spec: {containers: [{name: a, image: busybox}]}
EOF2
}
