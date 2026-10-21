# Shared by up.sh and smoke.sh: the Gitea API and git over a port-forward,
# with the admin credentials from the Secret gitea-admin.
ns=gitea
gitea_port="${GITEA_PORT:-3301}"
gitea_url="http://127.0.0.1:$gitea_port"
api="$gitea_url/api/v1"
# What Argo CD and the runners use.
gitea_internal="http://gitea-http.gitea.svc.cluster.local:3000"

secret_value() { kubectl -n "$1" get secret "$2" -o jsonpath="{.data.$3}" | base64 -d; }

# Port-forward to Gitea until the script exits.
gitea_connect() {
  GITEA_USER="$(secret_value "$ns" gitea-admin username)"
  GITEA_PASS="$(secret_value "$ns" gitea-admin password)"
  export GITEA_USER GITEA_PASS
  kubectl -n "$ns" port-forward svc/gitea-http "$gitea_port:3000" >/dev/null 2>&1 &
  gitea_pf=$!
  trap 'kill $gitea_pf 2>/dev/null; wait $gitea_pf 2>/dev/null || true' EXIT
  for _ in $(seq 50); do curl -fs "$api/version" >/dev/null 2>&1 && return; sleep 0.2; done
  echo "gitea: no answer on $gitea_url" >&2
  return 1
}

# The API as admin; credentials go to curl on stdin, not on its command line.
gitea() { printf 'user = "%s:%s"\n' "$GITEA_USER" "$GITEA_PASS" | curl -fsS -K - -H 'Content-Type: application/json' "$@"; }

# git as admin, credentials from the environment.
gitc() {
  GIT_TERMINAL_PROMPT=0 git -c credential.helper= \
    -c credential.helper='!f() { echo "username=$GITEA_USER"; echo "password=$GITEA_PASS"; }; f' \
    -c user.name="litek8s platform" -c user.email=platform@lk.test "$@"
}

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
