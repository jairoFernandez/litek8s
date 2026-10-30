#!/usr/bin/env bash
# GitOps on a litek8s dev cluster with litecd: a dev cluster, litecd from its
# latest release, and an app of apps from this directory (a Helm chart and a
# Kustomize app in their own project, in sync waves). Waits until every app
# is Synced and Healthy, then checks podinfo answers and prints what the
# controller costs.
#
#   examples/litecd/up.sh
#
# CLUSTER         dev cluster name (default: gitops; context litek8s-<name>)
# REPO_URL        the git repository root.yaml points at (default: this
#                 project on GitHub; a fork to try your own changes)
# REVISION        branch, tag or commit in it (default: main)
# LITECD_VERSION  litecd release (default: the latest)
# LITECD_INSTALL  litecd's install.sh, a URL or a file (default: its main branch)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cluster=${CLUSTER:-gitops}
ctx=litek8s-$cluster
repo=${REPO_URL:-https://github.com/jairoFernandez/litek8s}
revision=${REVISION:-main}
install=${LITECD_INSTALL:-https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh}
k() { kubectl --context "$ctx" "$@"; }
say() { printf '\n==> %s\n' "$*" >&2; }

say "dev cluster $cluster"
litek8s dev up --name "$cluster" >&2

say "litecd"
if [[ -f $install ]]; then script=$(cat "$install"); else script=$(curl -fsSL "$install"); fi
KUBECTL="kubectl --context $ctx" sh -c "$script"

say "app of apps from $repo ($revision)"
sed -e "s|repoURL: https://github.com/jairoFernandez/litek8s|repoURL: $repo|" \
    -e "s|targetRevision: main|targetRevision: $revision|" "$here/gitops/root.yaml" | k apply -f - >&2

say "waiting for every Application to be Synced and Healthy"
want=$(( $(find "$here/gitops/apps" -name '*.yaml' -exec grep -l '^kind: Application$' {} + | wc -l) + 1 ))
for _ in $(seq 1 120); do
    states=$(k -n argocd get applications -o jsonpath='{range .items[*]}{.status.sync.status}/{.status.health.status}{"\n"}{end}' 2>/dev/null || true)
    ok=$(grep -c '^Synced/Healthy$' <<<"$states" || true)
    [[ $ok -ge $want ]] && break
    sleep 5
done
k -n argocd get applications >&2
[[ $ok -ge $want ]] || { echo "up.sh: $ok of $want apps Synced and Healthy after 10 min: kubectl --context $ctx -n argocd logs deploy/litecd" >&2; exit 1; }

say "podinfo"
port=$(k -n demo-podinfo get svc podinfo -o jsonpath='{.spec.ports[?(@.name=="http")].nodePort}')
curl -fsS "http://localhost:$port" | grep -o '"message": *"[^"]*"' >&2
echo "  http://localhost:$port" >&2

say "what the controller costs"
k -n argocd top pod 2>/dev/null >&2 || echo "  (metrics not ready yet: kubectl --context $ctx -n argocd top pod)" >&2

cat >&2 <<EOF

Done. Context $ctx:
  kubectl --context $ctx -n argocd get applications
  litek8s ui                 # 0: the Apps view (sync, diff, history)
  litek8s dev delete --name $cluster
EOF
