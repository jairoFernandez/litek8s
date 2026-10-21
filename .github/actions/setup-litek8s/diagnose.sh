#!/usr/bin/env bash
# What a failed CI step needs to see about the cluster: nodes, pods, recent
# events, the pods' descriptions and the dev cluster's own logs.
#   .github/actions/setup-litek8s/diagnose.sh [cluster-name]
name=${1:-ci}
t() { timeout 20 "$@" 2>&1 || true; }
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
echo "::group::litek8s dev status"; t litek8s dev status --name "$name"; echo "::endgroup::"
echo "::group::nodes and pods"; t kubectl get nodes,pods -A -o wide; echo "::endgroup::"
echo "::group::events"; t kubectl get events -A --sort-by=.lastTimestamp | tail -n 60; echo "::endgroup::"
echo "::group::containers waiting or failed"
t kubectl get pods -A -o go-template='{{range .items}}{{$p := .metadata.name}}{{range .status.containerStatuses}}{{with .state.waiting}}{{$p}}/{{.reason}}: {{.message}}{{"\n"}}{{end}}{{with .lastState.terminated}}{{$p}} last exit {{.exitCode}} {{.reason}}: {{.message}}{{"\n"}}{{end}}{{end}}{{end}}'
echo "::endgroup::"
echo "::group::pods not Running"
for p in $(timeout 20 kubectl get pods -A --field-selector=status.phase!=Running,status.phase!=Succeeded -o jsonpath='{range .items[*]}{.metadata.namespace}/{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  t kubectl describe pod -n "${p%%/*}" "${p#*/}"
done
echo "::endgroup::"
for f in $(find "${XDG_DATA_HOME:-$HOME/.local/share}/litek8s/dev" -maxdepth 4 -type f -name '*.log' 2>/dev/null) \
         $(find "${XDG_DATA_HOME:-$HOME/.local/share}/litek8s/dev" -path '*/crun/containers/*' -maxdepth 9 -type f -name 'crun.log' -size +0 2>/dev/null | head -5); do
  echo "::group::$f"; tail -n 200 "$f"; echo "::endgroup::"
done
