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
deadline=$((SECONDS + timeout))
while :; do
  out="$(apps)"
  grep -qv ' Synced  *Healthy$' <<<"$out" || break
  if [[ $SECONDS -ge $deadline ]]; then echo "gitops: timeout"; sed 's/^/  /' <<<"$out"; exit 1; fi
  sleep 5
done
echo "gitops: all Applications Synced and Healthy"
sed 's/^/  /' <<<"$out"
