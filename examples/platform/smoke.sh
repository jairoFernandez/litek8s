#!/usr/bin/env bash
# End-to-end check of up.sh; exits non-zero unless everything passes.
# 1. CI: (re)creates the repo lkadmin/ci-smoke with one workflow job per
#    runner mode (actions/checkout where the mode has node) and waits for them.
# 2. GitOps: waits until every Argo CD Application is Synced and Healthy.
#   examples/platform/smoke.sh [modes...]            # default: lkdocker
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
. "$dir/lib.sh"
repo=ci-smoke
modes=("${@:-lkdocker}")
timeout="${TIMEOUT:-600}"

gitea_connect

# ---- CI ----------------------------------------------------------------------

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

gitea -X DELETE "$api/repos/$GITEA_USER/$repo" >/dev/null 2>&1 || true
gitea -X POST "$api/user/repos" -d "{\"name\":\"$repo\",\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null
content="$(printf '%s\n' "$workflow" | base64 | tr -d '\n')"
gitea -X POST "$api/repos/$GITEA_USER/$repo/contents/.gitea/workflows/smoke.yaml" \
  -d "{\"content\":\"$content\",\"message\":\"smoke\",\"branch\":\"main\"}" >/dev/null
echo "ci: pushed $GITEA_USER/$repo with jobs: ${modes[*]}"

jobs() {
  local run
  run="$(gitea "$api/repos/$GITEA_USER/$repo/actions/runs" 2>/dev/null | json 'd["workflow_runs"][0]["id"]' 2>/dev/null)" || return 0
  gitea "$api/repos/$GITEA_USER/$repo/actions/runs/$run/jobs" 2>/dev/null |
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

# ---- GitOps ------------------------------------------------------------------

kubectl -n argocd get application registry >/dev/null 2>&1 || { echo "gitops: no Argo CD registry, skipped"; exit 0; }
apps() {
  kubectl -n argocd get applications -o json |
    json '"\n".join("%-28s %-10s %s" % (a["metadata"]["name"], a.get("status",{}).get("sync",{}).get("status","?"), a.get("status",{}).get("health",{}).get("status","?")) for a in d["items"])'
}
# The app of apps on the repository's current commit first.
head="$(gitea "$api/repos/$GITEA_USER/gitops/branches/main" | json 'd["commit"]["id"]')"
kubectl -n argocd annotate application registry argocd.argoproj.io/refresh=normal --overwrite >/dev/null
deadline=$((SECONDS + timeout))
until [[ $(kubectl -n argocd get application registry -o jsonpath='{.status.sync.revision}') == "$head" ]]; do
  if [[ $SECONDS -ge $deadline ]]; then echo "gitops: registry not on $head (timeout)"; exit 1; fi
  sleep 3
done
while :; do
  out="$(apps)"
  grep -qv ' Synced  *Healthy$' <<<"$out" || break
  if [[ $SECONDS -ge $deadline ]]; then echo "gitops: timeout"; sed 's/^/  /' <<<"$out"; exit 1; fi
  sleep 5
done
echo "gitops: all Applications Synced and Healthy"
sed 's/^/  /' <<<"$out"

# ---- HTTPS through the ingress, verified with the platform's CA ----------------

domain="${DOMAIN:-localhost}"
port="$(kubectl -n traefik get svc traefik -o jsonpath='{.spec.ports[?(@.name=="websecure")].port}')"
ca="$(mktemp)"
trap 'kill $gitea_pf 2>/dev/null; wait $gitea_pf 2>/dev/null || true; rm -f "$ca"' EXIT
secret_value cert-manager platform-ca 'ca\.crt' >"$ca"
deadline=$((SECONDS + timeout))
urls=("https://argocd.$domain:$port/healthz" "https://gitea.$domain:$port/api/healthz")
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

# ---- The sample app: a push to metaphor reaches development ---------------------

if kubectl -n argocd get application development >/dev/null 2>&1; then
  msg="smoke-$RANDOM$RANDOM"
  m="$api/repos/$GITEA_USER/metaphor/contents/index.html"
  cur="$(gitea "$m")"
  html="$(json 'd["content"]' <<<"$cur" | base64 -d | sed "s#<p>message: .*</p>#<p>message: $msg</p>#")"
  commit="$(gitea -X PUT "$m" -d "{\"sha\":\"$(json 'd["sha"]' <<<"$cur")\",\"message\":\"$msg\",
    \"content\":\"$(printf '%s\n' "$html" | base64 | tr -d '\n')\"}" | json 'd["commit"]["sha"][:12]')"
  echo "metaphor: pushed $commit ($msg)"
  url="https://metaphor-development.$domain:$port/"
  deadline=$((SECONDS + timeout))
  until page="$(curl -s --cacert "$ca" "$url")" && grep -q "$msg" <<<"$page" && grep -q "version: $commit" <<<"$page"; do
    if [[ $SECONDS -ge $deadline ]]; then echo "metaphor: $url does not show $commit (timeout)"; printf '%s\n' "$page" | tail -3; exit 1; fi
    sleep 5
  done
  echo "metaphor: $url serves $commit (built by Gitea Actions, deployed by Argo CD)"
fi

# ---- Atlantis: a pull request plans and applies OpenTofu against OpenBao -------

kubectl get namespace atlantis >/dev/null 2>&1 || exit 0
name="smoke-$RANDOM"
file="terraform/openbao/$name.tf"
r="$api/repos/$GITEA_USER/gitops"
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }
# Wait for the atlantis comment number $1 (counting from 1) and print it.
atlantis_comment() {
  local n=$1 deadline=$((SECONDS + timeout)) out
  while :; do
    out="$(gitea "$r/issues/$pr/comments" | json "'\n'.join(c['body'] for c in [c for c in d if c['user']['login']=='atlantis'][$((n - 1)):$n])" 2>/dev/null || true)"
    [[ -n $out ]] && { printf '%s\n' "$out"; return; }
    if [[ $SECONDS -ge $deadline ]]; then echo "atlantis: no comment $n (timeout)" >&2; return 1; fi
    sleep 5
  done
}
expect() { grep -q "$1" <<<"$2" || { echo "atlantis: expected '$1' in:"; printf '%s\n' "$2" | head -40; exit 1; }; }
bao_root() { kubectl -n openbao exec openbao-0 -c openbao -- env BAO_ADDR=http://127.0.0.1:8200 BAO_TOKEN="$(secret_value openbao openbao-init root-token)" bao "$@"; }

gitea -X POST "$r/contents/$file" -d "{\"branch\":\"main\",\"new_branch\":\"$name\",\"message\":\"$name: add a policy\",
  \"content\":\"$(b64 "resource \"vault_policy\" \"$name\" {
  name   = \"$name\"
  policy = \"path \\\"secret/data/$name/*\\\" { capabilities = [\\\"read\\\"] }\"
}
")\"}" >/dev/null
pr="$(gitea -X POST "$r/pulls" -d "{\"head\":\"$name\",\"base\":\"main\",\"title\":\"$name\"}" | json 'd["number"]')"
echo "atlantis: opened pull request #$pr ($file)"

c="$(atlantis_comment 1)"; expect "Ran Plan" "$c"; expect "to add" "$c"
echo "atlantis: planned"
gitea -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis apply -p openbao"}' >/dev/null
c="$(atlantis_comment 2)"; expect "Apply complete" "$c"
bao_root policy read "$name" >/dev/null || { echo "atlantis: policy $name not in OpenBao"; exit 1; }
echo "atlantis: applied, policy $name is in OpenBao"

# Undo on the same branch, so the state matches main again, then close it.
sha="$(gitea "$r/contents/$file?ref=$name" | json 'd["sha"]')"
gitea -X DELETE "$r/contents/$file" -d "{\"branch\":\"$name\",\"sha\":\"$sha\",\"message\":\"$name: remove it\"}" >/dev/null
# No autoplan now: the pull request no longer differs from main.
sleep 5
gitea -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis plan -p openbao"}' >/dev/null
c="$(atlantis_comment 3)"; expect "1 to destroy" "$c"
gitea -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis apply -p openbao"}' >/dev/null
c="$(atlantis_comment 4)"; expect "Apply complete" "$c"
# Atlantis does not release its lock when Gitea closes or merges (README).
gitea -X POST "$r/issues/$pr/comments" -d '{"body":"atlantis unlock"}' >/dev/null
c="$(atlantis_comment 5)"; expect "unlocked" "$c"
gitea -X PATCH "$r/pulls/$pr" -d '{"state":"closed"}' >/dev/null
gitea -X DELETE "$r/branches/$name" >/dev/null
echo "atlantis: removed again and closed #$pr"
