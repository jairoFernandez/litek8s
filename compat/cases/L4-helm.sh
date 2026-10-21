# L4: real Helm charts install and become ready (`helm install --wait`).
# One case per line of charts.txt, registered dynamically.

COMPAT_CHARTS_FILE=${COMPAT_CHARTS_FILE:-"$COMPAT_DIR/charts.txt"}

while read -r release repo chart extra; do
    [[ -z "$release" || "$release" == \#* ]] && continue
    case_def "helm_${release//-/_}" L4 420 "helm install $release ($chart)"
    eval "t_helm_${release//-/_}() { helm_case '$release' '$repo' '$chart' $extra; }"
done <"$COMPAT_CHARTS_FILE"

helm_case() {
    local release=$1 repo=$2 chart=$3
    shift 3
    use_ns
    # COMPAT_HELM_NOWAIT=1 only checks that the API accepts every object of the
    # chart (an API-level sub-score); the real score waits for readiness.
    local wait=(--wait --timeout "${COMPAT_HELM_TIMEOUT:-6m}")
    [[ -n "${COMPAT_HELM_NOWAIT:-}" ]] && wait=()
    # ${a[@]+...}: bash 3.2 with `set -u` treats an empty array as unbound.
    local args=(install "$release" --namespace "$NS" ${wait[@]+"${wait[@]}"} "$@")
    if [[ "$chart" == - ]]; then
        args+=("$repo")
    else
        args+=("$chart" --repo "$repo")
    fi
    with_timeout 400 helm "${args[@]}" >/dev/null
    # Record which chart version was installed (shown in the case detail).
    helm -n "$NS" list -o json | sed -n 's/.*"chart":"\([^"]*\)".*/\1/p' >&2
    with_timeout 120 helm -n "$NS" uninstall "$release" ${wait[@]+--wait} >/dev/null
}

# Argo CD syncing with server-side apply and server-side diff builds its
# structured-merge-diff types from /openapi/v2 (client-go OpenAPISchema(), in
# protobuf): every built-in kind must be there with its group-version-kind.
case_def argocd_ssa L4 900 "Argo CD syncs a chart with ServerSideApply=true and ServerSideDiff"
t_argocd_ssa() {
    use_ns
    # Argo CD's CRDs may be left from the argo-cd chart case (crds.keep).
    local crds=(--set crds.keep=false)
    k get crd applications.argoproj.io >/dev/null 2>&1 && crds=(--set crds.install=false)
    with_timeout 480 helm install argocd-ssa argo-cd --repo https://argoproj.github.io/argo-helm --version 10.10.5 \
        --namespace "$NS" --wait --timeout 7m \
        --set dex.enabled=false --set notifications.enabled=false --set applicationSet.replicas=0 "${crds[@]}" >/dev/null
    k -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: podinfo
  annotations:
    argocd.argoproj.io/compare-options: ServerSideDiff=true
spec:
  project: default
  source:
    repoURL: https://stefanprodan.github.io/podinfo
    chart: podinfo
    targetRevision: 6.15.0
    helm:
      valuesObject:
        serviceAccount: {enabled: true}
  destination:
    server: https://kubernetes.default.svc
    namespace: $NS
  syncPolicy:
    automated: {}
    syncOptions: [ServerSideApply=true]
EOF
    if ! wait_for 360 argocd_app_synced_healthy; then
        k -n "$NS" get application podinfo -o jsonpath='{.status.sync.status}/{.status.health.status} {.status.conditions}' >&2
        echo >&2
        return 1
    fi
    # Applied server-side by Argo CD, not created client-side.
    expect_contains "$(k -n "$NS" get serviceaccount podinfo -o jsonpath='{range .metadata.managedFields[*]}{.manager}:{.operation} {end}')" "argocd-controller:Apply"
    k -n "$NS" delete application podinfo --wait=false >/dev/null
    with_timeout 180 helm -n "$NS" uninstall argocd-ssa --wait >/dev/null
}

argocd_app_synced_healthy() {
    [[ "$(k -n "$NS" get application podinfo -o jsonpath='{.status.sync.status}/{.status.health.status}')" == Synced/Healthy ]]
}
