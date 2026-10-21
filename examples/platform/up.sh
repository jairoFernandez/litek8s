#!/usr/bin/env bash
# A self-hosted GitOps platform on the current kube context: Gitea (Git
# server, Actions) with act_runner, the gitops repository in Gitea, and Argo
# CD syncing it. Safe to run again: secrets are kept, the repository is left
# as it is (SEED=update commits this checkout's gitops/ into it).
#   examples/platform/up.sh
#   RUNNERS="lkdocker dind host" CLUSTER_NAME=home DOMAIN=home.lan examples/platform/up.sh
# Web UIs are https://<app>.$DOMAIN (default localhost: browsers and curl
# resolve *.localhost to this machine) on the ingress's LoadBalancer ports
# HTTP_PORT/HTTPS_PORT (default 80/443), with certificates from the
# platform's own CA.
# Runner modes: lkdocker (litek8s Docker API), dind (own dockerd), host (in the pod).
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
. "$dir/lib.sh"
runners="${RUNNERS:-lkdocker}"
cluster="${CLUSTER_NAME:-mgmt}"
domain="${DOMAIN:-localhost}"
http_port="${HTTP_PORT:-80}"
https_port="${HTTPS_PORT:-443}"

# ---- Gitea and runners ---------------------------------------------------

kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
if ! kubectl -n "$ns" get secret gitea-admin >/dev/null 2>&1; then
  kubectl -n "$ns" create secret generic gitea-admin \
    --from-literal=username=lkadmin --from-literal=password="$(openssl rand -hex 16)" >/dev/null
fi
helm upgrade --install gitea gitea --repo https://dl.gitea.com/charts/ --version 12.7.0 \
  -n "$ns" -f "$dir/gitea/values.yaml" -f - --wait --timeout 10m >/dev/null <<EOF
ingress:
  enabled: true
  annotations:
    cert-manager.io/cluster-issuer: platform-ca
  hosts:
    - host: gitea.$domain
      paths: [{path: /}]
  tls:
    - secretName: gitea-tls
      hosts: [gitea.$domain]
EOF
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
find "$render" -name '*.yaml' -exec perl -pi -e "s#<CLUSTER_NAME>#$cluster#g; s#<GITOPS_REPO_URL>#$repo_url#g;
  s#<DOMAIN_NAME>#$domain#g; s#<INGRESS_HTTP_PORT>#$http_port#g; s#<INGRESS_HTTPS_PORT>#$https_port#g" {} +

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

# OpenBao's static seal key, before Argo CD deploys OpenBao: it unseals
# itself with it on every start.
kubectl create namespace openbao --dry-run=client -o yaml | kubectl apply -f - >/dev/null
if ! kubectl -n openbao get secret openbao-unseal-key >/dev/null 2>&1; then
  openssl rand 32 >"$work/key"
  kubectl -n openbao create secret generic openbao-unseal-key --from-file=key="$work/key" >/dev/null
  rm -f "$work/key"
fi

kubectl apply -f "$render/registry/clusters/$cluster/registry.yaml" >/dev/null
# Read the repository now, not at the next poll (3 min).
kubectl -n argocd annotate application registry argocd.argoproj.io/refresh=normal --overwrite >/dev/null
echo "registry: applied, syncing (examples/platform/smoke.sh waits for it)"

# ---- OpenBao -----------------------------------------------------------------

# What kubefirst's API did for Vault: initialize once (root token and
# recovery key into the Secret openbao-init), then the KV store `secret/`
# and Kubernetes auth for external-secrets, read-only. Idempotent.
echo "openbao: waiting for Argo CD to deploy it"
for _ in $(seq 120); do
  kubectl -n openbao get pod openbao-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null | grep -q true && break
  sleep 5
done
bao() { kubectl -n openbao exec -i openbao-0 -c openbao -- env BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN="${root:-}" bao "$@"; }
root=""
if [[ $(bao status -format=json 2>/dev/null | json 'd["initialized"]') == False ]]; then
  bao operator init -recovery-shares=1 -recovery-threshold=1 -format=json >"$work/init.json"
  kubectl -n openbao create secret generic openbao-init \
    --from-literal=root-token="$(json 'd["root_token"]' <"$work/init.json")" \
    --from-literal=recovery-key="$(json 'd["recovery_keys_b64"][0]' <"$work/init.json")" >/dev/null
  rm -f "$work/init.json"
  echo "openbao: initialized"
fi
root="$(secret_value openbao openbao-init root-token)"
bao secrets list -format=json | json '"secret/" in d' | grep -q True || bao secrets enable -path=secret kv-v2 >/dev/null
bao auth list -format=json | json '"kubernetes/" in d' | grep -q True || bao auth enable kubernetes >/dev/null
bao write auth/kubernetes/config kubernetes_host=https://kubernetes.default.svc >/dev/null
bao policy write external-secrets - >/dev/null <<EOF
path "secret/data/*" { capabilities = ["read"] }
path "secret/metadata/*" { capabilities = ["read", "list"] }
EOF
bao write auth/kubernetes/role/external-secrets \
  bound_service_account_names=external-secrets-operator \
  bound_service_account_namespaces=external-secrets-operator \
  policies=external-secrets ttl=1h >/dev/null
echo "openbao: ready (KV secret/, Kubernetes auth for external-secrets)"

port=""; [[ $https_port == 443 ]] || port=":$https_port"
cat <<EOF

Gitea:   https://gitea.$domain$port   user lkadmin, password:
           kubectl -n $ns get secret gitea-admin -o jsonpath='{.data.password}' | base64 -d
Argo CD: https://argocd.$domain$port  user admin, password:
           kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d
OpenBao: https://openbao.$domain$port root token:
           kubectl -n openbao get secret openbao-init -o jsonpath='{.data.root-token}' | base64 -d
CA:      kubectl -n cert-manager get secret platform-ca -o jsonpath='{.data.ca\.crt}' | base64 -d > platform-ca.crt
         (trust it in your browser or keychain to drop the certificate warning)
Check:   DOMAIN=$domain $dir/smoke.sh $runners
EOF
