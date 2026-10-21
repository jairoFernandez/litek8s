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
trap 'kill $gitea_pf 2>/dev/null; wait $gitea_pf 2>/dev/null; rm -f "$ca"' EXIT
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
