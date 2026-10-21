#!/usr/bin/env bash
# A self-hosted GitOps platform on the current kube context: Gitea (Git
# server, Actions) with act_runner, the gitops repository in Gitea, and Argo
# CD syncing it. Safe to run again: secrets are kept, the repository is left
# as it is (SEED=update commits this checkout's gitops/ into it).
#   examples/platform/up.sh
#   RUNNERS="lkdocker dind host" CLUSTER_NAME=home examples/platform/up.sh
# Runner modes: lkdocker (litek8s Docker API), dind (own dockerd), host (in the pod).
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
. "$dir/lib.sh"
runners="${RUNNERS:-lkdocker}"
cluster="${CLUSTER_NAME:-mgmt}"

# ---- Gitea and runners ---------------------------------------------------

kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
if ! kubectl -n "$ns" get secret gitea-admin >/dev/null 2>&1; then
  kubectl -n "$ns" create secret generic gitea-admin \
    --from-literal=username=lkadmin --from-literal=password="$(openssl rand -hex 16)" >/dev/null
fi
helm upgrade --install gitea gitea --repo https://dl.gitea.com/charts/ --version 12.7.0 \
  -n "$ns" -f "$dir/gitea/values.yaml" --wait --timeout 10m >/dev/null
echo "gitea: ready"

if ! kubectl -n "$ns" get secret runner-token >/dev/null 2>&1; then
  token="$(kubectl -n "$ns" exec deploy/gitea -c gitea -- gitea actions generate-runner-token | tail -1)"
  kubectl -n "$ns" create secret generic runner-token --from-literal=token="$token" >/dev/null
fi
for r in $runners; do kubectl apply -f "$dir/gitea/runner-$r.yaml" >/dev/null; done
for r in $runners; do
  kubectl -n "$ns" rollout status "deploy/runner-$r" --timeout 10m >/dev/null
  echo "runner-$r: ready"
done

# ---- the gitops repository -------------------------------------------------

gitea_connect
repo_url="$gitea_internal/$GITEA_USER/gitops.git"
work="$(mktemp -d)"
trap 'kill $gitea_pf 2>/dev/null; wait $gitea_pf 2>/dev/null; rm -rf "$work"' EXIT

# gitops/ with the tokens filled in, as the repository holds it.
render="$work/render"
mkdir -p "$render/registry/clusters/$cluster"
cp "$dir/gitops/NOTICE.md" "$render/"
cp -R "$dir/gitops/cluster/." "$render/registry/clusters/$cluster/"
find "$render" -name '*.yaml' -exec perl -pi -e "s#<CLUSTER_NAME>#$cluster#g; s#<GITOPS_REPO_URL>#$repo_url#g" {} +

gitea -X POST "$api/user/repos" -d '{"name":"gitops","private":true,"default_branch":"main"}' >/dev/null 2>&1 || true
empty="$(gitea "$api/repos/$GITEA_USER/gitops" | json 'd["empty"]')"
if [[ $empty == True || ${SEED:-} == update ]]; then
  repo="$work/repo"
  if [[ $empty == True ]]; then
    git init -q -b main "$repo"
    git -C "$repo" remote add origin "$gitea_url/$GITEA_USER/gitops.git"
  else
    gitc clone -q "$gitea_url/$GITEA_USER/gitops.git" "$repo"
    find "$repo" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
  fi
  cp -R "$render/." "$repo/"
  git -C "$repo" add -A
  if ! git -C "$repo" diff --cached --quiet; then
    gitc -C "$repo" commit -qm "litek8s platform: $cluster from examples/platform/gitops"
    gitc -C "$repo" push -q origin main
    echo "gitops: pushed to $GITEA_USER/gitops"
  fi
fi

# ---- Argo CD ---------------------------------------------------------------

# Once; then Argo CD manages itself from the repository.
if ! kubectl -n argocd get deploy argocd-server >/dev/null 2>&1; then
  kubectl apply --server-side --force-conflicts -k "$render/registry/clusters/$cluster/components/argocd" >/dev/null
fi
kubectl -n argocd rollout status deploy/argocd-repo-server --timeout 10m >/dev/null
kubectl -n argocd rollout status statefulset/argocd-application-controller --timeout 10m >/dev/null
echo "argocd: ready"

# Read-only access to the repository with a Gitea token of its own.
if ! kubectl -n argocd get secret repo-gitops >/dev/null 2>&1; then
  gitea -X DELETE "$api/users/$GITEA_USER/tokens/argocd" >/dev/null 2>&1 || true
  token="$(gitea -X POST "$api/users/$GITEA_USER/tokens" -d '{"name":"argocd","scopes":["read:repository"]}' | json 'd["sha1"]')"
  kubectl -n argocd create secret generic repo-gitops --from-literal=type=git --from-literal=url="$repo_url" \
    --from-literal=username="$GITEA_USER" --from-literal=password="$token" >/dev/null
  kubectl -n argocd label secret repo-gitops argocd.argoproj.io/secret-type=repository >/dev/null
fi

kubectl apply -f "$render/registry/clusters/$cluster/registry.yaml" >/dev/null
echo "registry: applied, syncing (examples/platform/smoke.sh waits for it)"

cat <<EOF

Gitea:   kubectl -n $ns port-forward svc/gitea-http 3000:3000   -> http://127.0.0.1:3000
         user lkadmin, password: kubectl -n $ns get secret gitea-admin -o jsonpath='{.data.password}' | base64 -d
Argo CD: kubectl -n argocd port-forward svc/argocd-server 8080:80  -> http://127.0.0.1:8080
         user admin, password: kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d
Check:   $dir/smoke.sh $runners
EOF
