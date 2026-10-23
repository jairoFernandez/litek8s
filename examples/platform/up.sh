#!/usr/bin/env bash
# A self-hosted GitOps platform on the current kube context: a Git provider
# (Gitea in the cluster, or GitHub) with Actions runners in the cluster, the
# gitops repository there, and Argo CD syncing it. Safe to run again:
# secrets are kept, the repository is left as it is (SEED=update commits this
# checkout's gitops/ into it, keeping registry/environments).
#   examples/platform/up.sh
#   GIT_PROVIDER=github GITHUB_TOKEN_FILE=~/.github-token examples/platform/up.sh
#   RUNNERS="lkdocker dind host" CLUSTER_NAME=home DOMAIN=home.lan examples/platform/up.sh
#
# GIT_PROVIDER     gitea (default): Gitea, act_runner, Atlantis on Gitea
#                  github: repositories on GitHub (GITHUB_OWNER, default the
#                  token's user), a self-hosted GitHub runner; GITHUB_TOKEN or
#                  GITHUB_TOKEN_FILE (README for its permissions); needs gh
# GITOPS_REPO      the gitops repository's name (default gitops)
# APP_REPO         the sample app's repository (default metaphor)
# PLATFORM_MODE    lk (default): CI builds with litek8s's Docker API into the
#                  cluster's image store, no registry. (dind: not yet, it
#                  needs nodes that pull from a registry with a private CA.)
# RUNNERS          Gitea's act_runner modes: lkdocker (default), dind, host
# DOMAIN           web UIs at https://<app>.$DOMAIN (default localhost:
#                  browsers and curl resolve *.localhost to this machine)
# HTTP_PORT/HTTPS_PORT  the ingress's LoadBalancer ports (default 80/443)
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
. "$dir/lib.sh"
git_provider="${GIT_PROVIDER:-gitea}"
platform_mode="${PLATFORM_MODE:-lk}"
gitops_repo="${GITOPS_REPO:-gitops}"
app_repo="${APP_REPO:-metaphor}"
runners="${RUNNERS:-lkdocker}"
cluster="${CLUSTER_NAME:-mgmt}"
domain="${DOMAIN:-localhost}"
http_port="${HTTP_PORT:-80}"
https_port="${HTTPS_PORT:-443}"

case "$platform_mode" in
  lk) ;;
  dind) echo "PLATFORM_MODE=dind: not yet (README, \"Platform modes\")" >&2; exit 1 ;;
  *) echo "PLATFORM_MODE: lk, not $platform_mode" >&2; exit 1 ;;
esac
case "$git_provider" in
  gitea) ;;
  github) command -v gh >/dev/null || { echo "GIT_PROVIDER=github needs the gh CLI, logged in" >&2; exit 1; } ;;
  *) echo "GIT_PROVIDER: gitea or github, not $git_provider" >&2; exit 1 ;;
esac

work="$(mktemp -d)"
trap 'scm_disconnect; rm -rf "$work"' EXIT

# ---- Gitea and its runners (GIT_PROVIDER=gitea) -------------------------------

if [[ $git_provider == gitea ]]; then
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
fi

scm_connect
repo_url="$repo_base/$gitops_repo.git"

# ---- the gitops repository -----------------------------------------------------

# gitops/ with the tokens filled in, as the repository holds it; only the
# provider's own pieces (Atlantis needs Gitea's webhooks, which GitHub cannot
# send into the cluster; the GitHub runner only GitHub).
render="$work/render"
mkdir -p "$render/registry/clusters/$cluster"
cp -R "$dir/gitops/." "$render/"
rm -rf "$render/cluster"
cp -R "$dir/gitops/cluster/." "$render/registry/clusters/$cluster/"
c="$render/registry/clusters/$cluster"
if [[ $git_provider == gitea ]]; then
  rm -rf "$c/github-runner.yaml" "$c/components/github-runner" "$render/.github"
  tofu_state_ns=atlantis
else
  rm -f "$c/atlantis.yaml" "$render/atlantis.yaml"
  tofu_state_ns=github-iac
fi
https_suffix=""; [[ $https_port == 443 ]] || https_suffix=":$https_port"
find "$render" \( -name '*.yaml' -o -name '*.tf' \) -exec perl -pi -e "s#<CLUSTER_NAME>#$cluster#g; s#<GITOPS_REPO_URL>#$repo_url#g;
  s#<DOMAIN_NAME>#$domain#g; s#<INGRESS_HTTP_PORT>#$http_port#g; s#<INGRESS_HTTPS_PORT>#$https_port#g;
  s#<INGRESS_HTTPS_SUFFIX>#$https_suffix#g; s#<APP_REPOSITORY>#$owner/$app_repo#g;
  s#<GITOPS_REPOSITORY>#$owner/$gitops_repo#g; s#<TOFU_STATE_NAMESPACE>#$tofu_state_ns#g" {} +

scm_ensure_repo "$gitops_repo"
empty=false; scm_repo_empty "$gitops_repo" && empty=true
if [[ $empty == true || ${SEED:-} == update ]]; then
  repo="$work/repo"
  if [[ $empty == true ]]; then
    git init -q -b main "$repo"
  else
    scm_git clone -q "$push_base/$gitops_repo.git" "$repo"
    # The environments stay: their image tags are what the apps' CI set.
    [[ -d $repo/registry/environments ]] && mv "$repo/registry/environments" "$work/environments"
    find "$repo" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
  fi
  cp -R "$render/." "$repo/"
  if [[ -d $work/environments ]]; then
    rm -rf "$repo/registry/environments"
    mv "$work/environments" "$repo/registry/environments"
  fi
  git -C "$repo" add -A
  if ! git -C "$repo" diff --cached --quiet; then
    scm_git -C "$repo" commit -qm "litek8s platform: $cluster from examples/platform/gitops"
    scm_git -C "$repo" push -q "$push_base/$gitops_repo.git" main
    echo "gitops: pushed to $owner/$gitops_repo ($git_provider)"
  fi
fi

# ---- the sample app ------------------------------------------------------------

# Its CI (.github/workflows/ci.yaml, on Gitea Actions or GitHub Actions) builds
# it on a runner in the cluster and sets its tag in the gitops repository
# with the secret GITOPS_TOKEN; the variable GITOPS_REPO names that repository.
scm_ensure_repo "$app_repo"
if scm_repo_empty "$app_repo"; then
  if [[ $git_provider == gitea ]]; then
    scm -X POST "$api/users/$owner/tokens" -d "{\"name\":\"$app_repo-ci-$(date +%s)\",\"scopes\":[\"write:repository\"]}" |
      json 'd["sha1"]' | scm_set_secret "$app_repo" GITOPS_TOKEN
  else
    printf '%s' "$SCM_PASS" | scm_set_secret "$app_repo" GITOPS_TOKEN
  fi
  scm_set_variable "$app_repo" GITOPS_REPO "$gitops_repo"
  app="$work/app"
  git init -q -b main "$app"
  cp -R "$dir/metaphor/." "$app/"
  git -C "$app" add -A
  scm_git -C "$app" commit -qm "metaphor: the platform's sample app"
  scm_git -C "$app" push -q "$push_base/$app_repo.git" main
  echo "app: pushed to $owner/$app_repo (its CI builds and deploys it)"
fi

# ---- Argo CD -------------------------------------------------------------------

# Once; then Argo CD manages itself from the repository.
if ! kubectl -n argocd get deploy argocd-server >/dev/null 2>&1; then
  kubectl apply --server-side --force-conflicts -k "$render/registry/clusters/$cluster/components/argocd" >/dev/null
fi
kubectl -n argocd rollout status deploy/argocd-repo-server --timeout 10m >/dev/null
kubectl -n argocd rollout status statefulset/argocd-application-controller --timeout 10m >/dev/null
echo "argocd: ready"

kubectl -n argocd create configmap platform --from-literal=provider="$git_provider" --from-literal=owner="$owner" \
  --from-literal=gitops_repo="$gitops_repo" --from-literal=app_repo="$app_repo" --from-literal=domain="$domain" \
  --from-literal=https_port="$https_port" --from-literal=mode="$platform_mode" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# Read access to the gitops repository: a Gitea token of its own, or the
# GitHub token.
if ! kubectl -n argocd get secret repo-gitops >/dev/null 2>&1; then
  if [[ $git_provider == gitea ]]; then
    scm -X DELETE "$api/users/$owner/tokens/argocd" >/dev/null 2>&1 || true
    token="$(scm -X POST "$api/users/$owner/tokens" -d '{"name":"argocd","scopes":["read:repository"]}' | json 'd["sha1"]')"
  else
    token="$SCM_PASS"
  fi
  kubectl -n argocd create secret generic repo-gitops --from-literal=type=git --from-literal=url="$repo_url" \
    --from-literal=username="$SCM_USER" --from-literal=password="$token" >/dev/null
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
# Gitea tells Argo CD about pushes at once; GitHub cannot reach the cluster,
# there Argo CD polls (3 min).
if [[ $git_provider == gitea ]] &&
  ! scm "$api/repos/$owner/$gitops_repo/hooks" | json '[h["config"]["url"] for h in d]' | grep -q argocd-server; then
  scm -X POST "$api/repos/$owner/$gitops_repo/hooks" -d '{"type":"gogs","active":true,"events":["push"],
    "config":{"url":"http://argocd-server.argocd.svc.cluster.local/api/webhook","content_type":"json"}}' >/dev/null
fi
kubectl -n argocd annotate application registry argocd.argoproj.io/refresh=normal --overwrite >/dev/null
echo "registry: applied, syncing (examples/platform/smoke.sh waits for it)"

# ---- OpenBao ---------------------------------------------------------------------

# Initialize it once (root token and recovery key into the Secret
# openbao-init), then the KV stores and Kubernetes auth for external-secrets,
# read-only. Idempotent.
echo "openbao: waiting for Argo CD to deploy it"
for _ in $(seq 120); do
  kubectl -n openbao get pod openbao-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null | grep -q true && break
  sleep 5
done
if ! kubectl -n openbao get pod openbao-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null | grep -q true; then
  echo "openbao: not ready after 10 minutes; Argo CD says:" >&2
  kubectl -n argocd get application openbao -o jsonpath='  sync {.status.sync.status}, health {.status.health.status}{"\n"}  {.status.conditions[*].message}{"\n"}' >&2 || true
  exit 1
fi
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
# platform/: the platform's own credentials, through the ClusterSecretStore
# openbao-platform only (limited to the namespaces it lists).
bao secrets list -format=json | json '"platform/" in d' | grep -q True || bao secrets enable -path=platform kv-v2 >/dev/null
bao policy write external-secrets-platform - >/dev/null <<EOF
path "platform/data/*" { capabilities = ["read"] }
path "platform/metadata/*" { capabilities = ["read", "list"] }
EOF
bao write auth/kubernetes/role/external-secrets-platform \
  bound_service_account_names=external-secrets-operator \
  bound_service_account_namespaces=external-secrets-operator \
  policies=external-secrets-platform ttl=1h >/dev/null
echo "openbao: ready (KV secret/ and platform/, Kubernetes auth for external-secrets)"

# ---- the provider's platform credentials -------------------------------------------

if [[ $git_provider == gitea ]]; then
  # Atlantis: its Gitea user (write access to the gitops repository, a token),
  # the repository's webhook to it, and its credentials in OpenBao's
  # platform/, where its ExternalSecret reads them. Once.
  if ! bao kv get -format=json platform/atlantis >/dev/null 2>&1; then
    apass="$(openssl rand -hex 16)"
    scm -X POST "$api/admin/users" -d "{\"username\":\"atlantis\",\"email\":\"atlantis@lk.test\",\"password\":\"$apass\",\"must_change_password\":false}" >/dev/null 2>&1 ||
      scm -X PATCH "$api/admin/users/atlantis" -d "{\"login_name\":\"atlantis\",\"source_id\":0,\"password\":\"$apass\",\"must_change_password\":false}" >/dev/null
    scm -X PUT "$api/repos/$owner/$gitops_repo/collaborators/atlantis" -d '{"permission":"write"}' >/dev/null
    atoken="$(printf 'user = "atlantis:%s"\n' "$apass" | curl -fsS -K - -H 'Content-Type: application/json' \
      -X POST "$api/users/atlantis/tokens" -d "{\"name\":\"atlantis-$(date +%s)\",\"scopes\":[\"write:repository\",\"write:issue\",\"read:user\"]}" | json 'd["sha1"]')"
    hook_secret="$(openssl rand -hex 20)"
    scm -X POST "$api/repos/$owner/$gitops_repo/hooks" -d "{\"type\":\"gitea\",\"active\":true,
      \"config\":{\"url\":\"http://atlantis.atlantis.svc.cluster.local/events\",\"content_type\":\"json\",\"secret\":\"$hook_secret\"},
      \"events\":[\"pull_request\",\"pull_request_sync\",\"pull_request_comment\",\"issue_comment\"]}" >/dev/null
    # OpenTofu manages OpenBao's policies through Atlantis: the root token.
    bao kv put platform/atlantis gitea_token="$atoken" gitea_secret="$hook_secret" VAULT_TOKEN="$root" >/dev/null
    echo "atlantis: Gitea user, token and webhook; credentials in OpenBao platform/atlantis"
  fi
else
  # The runners register themselves with the token; the infrastructure
  # runner also applies OpenTofu to OpenBao with the root token (it manages
  # policies), as Atlantis does with Gitea.
  printf '%s' "$SCM_PASS" | bao kv put platform/github GITHUB_TOKEN=- >/dev/null
  printf '%s' "$SCM_PASS" | bao kv put platform/github-iac GITHUB_TOKEN=- VAULT_TOKEN="$root" >/dev/null
  echo "github: tokens in OpenBao platform/github (app runner) and platform/github-iac (OpenTofu runner)"
fi

port=""; [[ $https_port == 443 ]] || port=":$https_port"
echo
if [[ $git_provider == gitea ]]; then
  cat <<EOF
Gitea:   https://gitea.$domain$port   user lkadmin, password:
           kubectl -n $ns get secret gitea-admin -o jsonpath='{.data.password}' | base64 -d
Atlantis: https://atlantis.$domain$port  (pull requests to $owner/$gitops_repo in Gitea)
EOF
else
  cat <<EOF
GitHub:  https://github.com/$owner/$gitops_repo and https://github.com/$owner/$app_repo
         (pull requests to $gitops_repo that change terraform/: tofu plan as a comment, apply on merge)
EOF
fi
cat <<EOF
Argo CD: https://argocd.$domain$port  user admin, password:
           kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d
Argo Workflows: https://argo.$domain$port  token:
           kubectl -n argo create token argo-workflows-server --duration 12h   (paste as "Bearer <token>")
App:     https://metaphor-development.$domain$port  (push to $owner/$app_repo to deploy)
OpenBao: https://openbao.$domain$port root token:
           kubectl -n openbao get secret openbao-init -o jsonpath='{.data.root-token}' | base64 -d
CA:      kubectl -n cert-manager get secret platform-ca -o jsonpath='{.data.ca\.crt}' | base64 -d > platform-ca.crt
         (trust it in your browser or keychain to drop the certificate warning)
Check:   $dir/smoke.sh $( [[ $git_provider == gitea ]] && echo "$runners")
EOF
