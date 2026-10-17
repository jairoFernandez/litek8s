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
