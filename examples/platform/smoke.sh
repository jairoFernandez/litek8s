#!/usr/bin/env bash
# End-to-end check of up.sh, with what it chose (ConfigMap argocd/platform);
# exits non-zero unless everything passes.
#   examples/platform/smoke.sh [act_runner modes...]   # Gitea; default: lkdocker
# With GIT_PROVIDER=github it reads the GitHub token from the runner's Secret
# unless GITHUB_TOKEN or GITHUB_TOKEN_FILE is set.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
. "$dir/lib.sh"
repo=ci-smoke
modes=("${@:-lkdocker}")
timeout="${TIMEOUT:-600}"
git_provider="$(platform_get provider)"; git_provider="${git_provider:-gitea}"
gitops_repo="$(platform_get gitops_repo)"; gitops_repo="${gitops_repo:-gitops}"
app_repo="$(platform_get app_repo)"; app_repo="${app_repo:-metaphor}"
domain="$(platform_get domain)"; domain="${domain:-${DOMAIN:-localhost}}"
if [[ $git_provider == github && -z ${GITHUB_TOKEN:-} && -z ${GITHUB_TOKEN_FILE:-} ]]; then
  GITHUB_TOKEN="$(secret_value github-runner github-runner GITHUB_TOKEN)"
fi
ca="$(mktemp)"
trap 'scm_disconnect; rm -f "$ca"' EXIT
scm_connect

# ---- CI: one job per act_runner mode (Gitea) -------------------------------------

if [[ $git_provider == gitea ]]; then
  workflow="name: smoke
on: [push]
jobs:"
  for m in "${modes[@]}"; do
    workflow+="
  $m:
    runs-on: $m
    steps:"
    [[ $m == host ]] || workflow+="
      - uses: actions/checkout@v4"
    workflow+="
      - run: echo \"mode=$m\" && uname -m && cat /etc/os-release | head -1"
  done

  scm -X DELETE "$api/repos/$owner/$repo" >/dev/null 2>&1 || true
  scm -X POST "$api/user/repos" -d "{\"name\":\"$repo\",\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null
  content="$(printf '%s\n' "$workflow" | base64 | tr -d '\n')"
  scm -X POST "$api/repos/$owner/$repo/contents/.gitea/workflows/smoke.yaml" \
    -d "{\"content\":\"$content\",\"message\":\"smoke\",\"branch\":\"main\"}" >/dev/null
  echo "ci: pushed $owner/$repo with jobs: ${modes[*]}"

  jobs() {
    local run
    run="$(scm "$api/repos/$owner/$repo/actions/runs" 2>/dev/null | json 'd["workflow_runs"][0]["id"]' 2>/dev/null)" || return 0
    scm "$api/repos/$owner/$repo/actions/runs/$run/jobs" 2>/dev/null |
      json '"\n".join("%s %s %s" % (j["name"], j["status"], j.get("conclusion") or "") for j in d.get("jobs") or [])'
  }
  deadline=$((SECONDS + timeout))
  while :; do
    out="$(jobs || true)"
    [[ $(grep -c ' completed ' <<<"$out" || true) -eq ${#modes[@]} ]] && break
    if [[ $SECONDS -ge $deadline ]]; then echo "ci: timeout"; echo "$out"; exit 1; fi
    sleep 5
  done
  sed 's/^/  /' <<<"$out"
  if grep -v ' completed success$' <<<"$out" | grep -q .; then echo "ci: FAILED"; exit 1; fi
fi

# ---- GitOps ------------------------------------------------------------------

kubectl -n argocd get application registry >/dev/null 2>&1 || { echo "gitops: no Argo CD registry, skipped"; exit 0; }
apps() {
  kubectl -n argocd get applications -o json |
    json '"\n".join("%-28s %-10s %s" % (a["metadata"]["name"], a.get("status",{}).get("sync",{}).get("status","?"), a.get("status",{}).get("health",{}).get("status","?")) for a in d["items"])'
}
# The app of apps on the repository's current commit first.
head="$(scm "$api/repos/$owner/$gitops_repo/branches/main" | json 'd["commit"].get("id") or d["commit"]["sha"]')"
kubectl -n argocd annotate application registry argocd.argoproj.io/refresh=normal --overwrite >/dev/null
deadline=$((SECONDS + timeout))
until [[ $(kubectl -n argocd get application registry -o jsonpath='{.status.sync.revision}') == "$head" ]]; do
  if [[ $SECONDS -ge $deadline ]]; then echo "gitops: registry not on $head (timeout)"; exit 1; fi
  sleep 3
done
# Every Application that reads the gitops repository on that commit too
# (without a webhook each polls on its own), then all Synced and Healthy.
stale() {
  kubectl -n argocd get applications -o json | json "' '.join(a['metadata']['name'] for a in d['items']
    if a['spec'].get('source',{}).get('repoURL','').rstrip('/').removesuffix('.git').endswith('/$gitops_repo')
    and a.get('status',{}).get('sync',{}).get('revision') != '$head')"
}
while :; do
  # The app's CI may commit meanwhile: the repository's commit of now.
  head="$(scm "$api/repos/$owner/$gitops_repo/branches/main" | json 'd["commit"].get("id") or d["commit"]["sha"]')"
  for a in $(stale); do kubectl -n argocd annotate application "$a" argocd.argoproj.io/refresh=normal --overwrite >/dev/null; done
  out="$(apps)"
  [[ -z $(stale) ]] && ! grep -qv ' Synced  *Healthy$' <<<"$out" && break
  if [[ $SECONDS -ge $deadline ]]; then echo "gitops: timeout"; sed 's/^/  /' <<<"$out"; exit 1; fi
  sleep 5
done
echo "gitops: all Applications Synced and Healthy"
sed 's/^/  /' <<<"$out"

# ---- HTTPS through the ingress, verified with the platform's CA ----------------

port="$(kubectl -n traefik get svc traefik -o jsonpath='{.spec.ports[?(@.name=="websecure")].port}')"
secret_value cert-manager platform-ca 'ca\.crt' >"$ca"
deadline=$((SECONDS + timeout))
urls=("https://argocd.$domain:$port/healthz")
[[ $git_provider == gitea ]] && urls+=("https://gitea.$domain:$port/api/healthz")
kubectl -n openbao get ingress openbao >/dev/null 2>&1 && urls+=("https://openbao.$domain:$port/v1/sys/health")
for url in "${urls[@]}"; do
  until code="$(curl -s -o /dev/null -w '%{http_code}' --cacert "$ca" "$url")" && [[ $code == 200 ]]; do
    if [[ $SECONDS -ge $deadline ]]; then echo "https: $url -> $code (timeout)"; exit 1; fi
    sleep 5
  done
  echo "https: $url 200 (certificate from platform-ca)"
done

# ---- Secrets: OpenBao -> external-secrets -> a Kubernetes Secret ---------------

kubectl -n openbao get secret openbao-init >/dev/null 2>&1 || { echo "secrets: no OpenBao, skipped"; exit 0; }
value="smoke-$RANDOM$RANDOM"
kubectl -n openbao exec openbao-0 -c openbao -- env BAO_ADDR=http://127.0.0.1:8200 \
  BAO_TOKEN="$(secret_value openbao openbao-init root-token)" bao kv put secret/smoke value="$value" >/dev/null
kubectl create namespace platform-smoke --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl apply -f - >/dev/null <<YAML
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: smoke
  namespace: platform-smoke
spec:
  refreshInterval: 10s
  secretStoreRef:
    kind: ClusterSecretStore
    name: openbao
  target:
    name: smoke
  data:
    - secretKey: value
      remoteRef:
        key: smoke
        property: value
YAML
deadline=$((SECONDS + 120))
until [[ $(secret_value platform-smoke smoke value 2>/dev/null) == "$value" ]]; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo "secrets: FAILED"; kubectl -n platform-smoke get externalsecret smoke -o jsonpath='{.status.conditions}'; echo; exit 1
  fi
  sleep 2
done
echo "secrets: OpenBao secret/smoke -> ExternalSecret -> Secret platform-smoke/smoke"

# ---- Argo Workflows: a workflow runs to completion -----------------------------

if kubectl get namespace argo >/dev/null 2>&1; then
  wf="$(kubectl create -o name -f - <<YAML
apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  generateName: smoke-
  namespace: argo
spec:
  serviceAccountName: argo-workflow
  entrypoint: main
  templates:
    - name: main
      steps:
        - - name: hello
            template: echo
            arguments: {parameters: [{name: msg, value: hello}]}
        - - name: world
            template: echo
            arguments: {parameters: [{name: msg, value: world}]}
    - name: echo
      inputs: {parameters: [{name: msg}]}
      container:
        image: busybox:1.37
        command: [echo, "{{inputs.parameters.msg}}"]
YAML
)"
  deadline=$((SECONDS + timeout))
  until phase="$(kubectl -n argo get "$wf" -o jsonpath='{.status.phase}')" && [[ $phase == Succeeded || $phase == Failed || $phase == Error ]]; do
    if [[ $SECONDS -ge $deadline ]]; then echo "workflows: $wf timeout ($phase)"; exit 1; fi
    sleep 3
  done
  [[ $phase == Succeeded ]] || { echo "workflows: $wf $phase"; kubectl -n argo get "$wf" -o jsonpath='{.status.message}'; echo; exit 1; }
  echo "workflows: $wf Succeeded (2 steps)"
  kubectl -n argo delete "$wf" >/dev/null
fi

# ---- The sample app: a push to its repository reaches development ---------------

if kubectl -n argocd get application development >/dev/null 2>&1; then
  msg="smoke-$RANDOM$RANDOM"
  m="$api/repos/$owner/$app_repo/contents/index.html"
  cur="$(scm "$m")"
  html="$(json 'd["content"]' <<<"$cur" | base64 -d | sed "s#<p>message: .*</p>#<p>message: $msg</p>#")"
  commit="$(scm -X PUT "$m" -d "{\"sha\":\"$(json 'd["sha"]' <<<"$cur")\",\"message\":\"$msg\",
    \"content\":\"$(printf '%s\n' "$html" | base64 | tr -d '\n')\"}" | json 'd["commit"]["sha"][:12]')"
  echo "app: pushed $commit to $owner/$app_repo ($msg)"
  url="https://metaphor-development.$domain:$port/"
  deadline=$((SECONDS + timeout))
  until page="$(curl -s --cacert "$ca" "$url")" && grep -q "$msg" <<<"$page" && grep -q "version: $commit" <<<"$page"; do
    if [[ $SECONDS -ge $deadline ]]; then echo "app: $url does not show $commit (timeout)"; printf '%s\n' "$page" | tail -3; exit 1; fi
    # GitHub cannot tell Argo CD about the CI's push: ask it to look.
    [[ $git_provider == github ]] && kubectl -n argocd annotate application development argocd.argoproj.io/refresh=normal --overwrite >/dev/null
    sleep 5
  done
  echo "app: $url serves $commit (built by $git_provider's CI on a runner in the cluster, deployed by Argo CD)"
fi

# ---- OpenTofu on GitHub Actions: a pull request plans, its merge applies --------

if [[ $git_provider == github ]]; then
  R="$owner/$gitops_repo"
  name="smoke-$RANDOM"
  file="terraform/openbao/$name.tf"
  bao_root() { kubectl -n openbao exec openbao-0 -c openbao -- env BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN="$(secret_value openbao openbao-init root-token)" bao "$@"; }
  # Wait for the tofu workflow's run on commit $1 and fail unless it succeeds.
  tofu_run() {
    local deadline=$((SECONDS + timeout)) out
    while :; do
      out="$(gh run list --repo "$R" --workflow tofu.yaml --commit "$1" --json status,conclusion --jq '.[0] | "\(.status) \(.conclusion)"' 2>/dev/null || true)"
      case "$out" in
        "completed success") return ;;
        completed*) echo "iac: run on $1: $out"; gh run view --repo "$R" "$(gh run list --repo "$R" --workflow tofu.yaml --commit "$1" --json databaseId --jq '.[0].databaseId')" --log-failed | tail -20; exit 1 ;;
      esac
      if [[ $SECONDS -ge $deadline ]]; then echo "iac: no finished run on $1 (timeout; $out)"; exit 1; fi
      sleep 5
    done
  }
  main_sha="$(gh api "repos/$R/git/ref/heads/main" --jq .object.sha)"
  gh api -X POST "repos/$R/git/refs" -f ref="refs/heads/$name" -f sha="$main_sha" >/dev/null
  content="$(printf 'resource "vault_policy" "%s" {\n  name   = "%s"\n  policy = "path \\"secret/data/%s/*\\" { capabilities = [\\"read\\"] }"\n}\n' "$name" "$name" "$name" | base64 | tr -d '\n')"
  head="$(gh api -X PUT "repos/$R/contents/$file" -f message="$name: add a policy" -f branch="$name" -f content="$content" --jq .commit.sha)"
  pr="$(gh pr create --repo "$R" --head "$name" --base main --title "$name" --body "smoke.sh" | grep -o '[0-9]*$')"
  echo "iac: opened pull request #$pr ($file)"
  tofu_run "$head"
  gh api "repos/$R/issues/$pr/comments" --jq '.[].body' | grep -q "tofu plan.*merge to apply" || { echo "iac: no plan comment on #$pr"; exit 1; }
  gh api "repos/$R/issues/$pr/comments" --jq '.[].body' | grep -q "to add" || { echo "iac: the plan adds nothing"; exit 1; }
  echo "iac: planned (comment on #$pr)"
  gh pr merge "$pr" --repo "$R" --squash --delete-branch >/dev/null
  merged="$(gh pr view "$pr" --repo "$R" --json mergeCommit --jq .mergeCommit.oid)"
  tofu_run "$merged"
  bao_root policy read "$name" >/dev/null || { echo "iac: policy $name not in OpenBao"; exit 1; }
  echo "iac: merged and applied, policy $name is in OpenBao"
  # Undo on main, so the repository and OpenBao end as they began.
  sha="$(gh api "repos/$R/contents/$file" --jq .sha)"
  removed="$(gh api -X DELETE "repos/$R/contents/$file" -f message="$name: remove it" -f sha="$sha" --jq .commit.sha)"
  tofu_run "$removed"
  if bao_root policy read "$name" >/dev/null 2>&1; then echo "iac: policy $name still in OpenBao"; exit 1; fi
  echo "iac: removed again (applied from main)"
fi

# ---- Atlantis: a pull request plans and applies OpenTofu against OpenBao -------

kubectl get namespace atlantis >/dev/null 2>&1 || exit 0
name="smoke-$RANDOM"
file="terraform/openbao/$name.tf"
r="$api/repos/$owner/$gitops_repo"
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }
# Wait for the atlantis comment number $1 (counting from 1) and print it.
atlantis_comment() {
  local n=$1 deadline=$((SECONDS + timeout)) out
  while :; do
    out="$(scm "$r/issues/$pr/comments" | json "'\n'.join(c['body'] for c in [c for c in d if c['user']['login']=='atlantis'][$((n - 1)):$n])" 2>/dev/null || true)"
    [[ -n $out ]] && { printf '%s\n' "$out"; return; }
    if [[ $SECONDS -ge $deadline ]]; then echo "atlantis: no comment $n (timeout)" >&2; return 1; fi
    sleep 5
  done
}
expect() { grep -q "$1" <<<"$2" || { echo "atlantis: expected '$1' in:"; printf '%s\n' "$2" | head -40; exit 1; }; }
bao_root() { kubectl -n openbao exec openbao-0 -c openbao -- env BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN="$(secret_value openbao openbao-init root-token)" bao "$@"; }

scm -X POST "$r/contents/$file" -d "{\"branch\":\"main\",\"new_branch\":\"$name\",\"message\":\"$name: add a policy\",
  \"content\":\"$(b64 "resource \"vault_policy\" \"$name\" {
  name   = \"$name\"
  policy = \"path \\\"secret/data/$name/*\\\" { capabilities = [\\\"read\\\"] }\"
}
")\"}" >/dev/null
pr="$(scm -X POST "$r/pulls" -d "{\"head\":\"$name\",\"base\":\"main\",\"title\":\"$name\"}" | json 'd["number"]')"
echo "atlantis: opened pull request #$pr ($file)"

c="$(atlantis_comment 1)"; expect "Ran Plan" "$c"; expect "to add" "$c"
echo "atlantis: planned"
scm -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis apply -p openbao"}' >/dev/null
c="$(atlantis_comment 2)"; expect "Apply complete" "$c"
bao_root policy read "$name" >/dev/null || { echo "atlantis: policy $name not in OpenBao"; exit 1; }
echo "atlantis: applied, policy $name is in OpenBao"

# Undo on the same branch, so the state matches main again, then close it.
sha="$(scm "$r/contents/$file?ref=$name" | json 'd["sha"]')"
scm -X DELETE "$r/contents/$file" -d "{\"branch\":\"$name\",\"sha\":\"$sha\",\"message\":\"$name: remove it\"}" >/dev/null
# No autoplan now: the pull request no longer differs from main.
sleep 5
scm -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis plan -p openbao"}' >/dev/null
c="$(atlantis_comment 3)"; expect "1 to destroy" "$c"
scm -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis apply -p openbao"}' >/dev/null
c="$(atlantis_comment 4)"; expect "Apply complete" "$c"
# Atlantis does not release its lock when Gitea closes or merges (README).
scm -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis unlock"}' >/dev/null
c="$(atlantis_comment 5)"; expect "unlocked" "$c"
scm -X PATCH "$r/pulls/$pr" -d '{"state":"closed"}' >/dev/null
scm -X DELETE "$r/branches/$name" >/dev/null
echo "atlantis: removed again and closed #$pr"
