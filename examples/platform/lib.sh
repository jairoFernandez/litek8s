# Shared by up.sh and smoke.sh: where the repositories live (Gitea in the
# cluster or GitHub) and how to reach them.
ns=gitea
gitea_port="${GITEA_PORT:-3301}"
gitea_url="http://127.0.0.1:$gitea_port"
# What Argo CD and the runners use.
gitea_internal="http://gitea-http.gitea.svc.cluster.local:3000"

secret_value() { kubectl -n "$1" get secret "$2" -o jsonpath="{.data.$3}" | base64 -d; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

# ---- the provider ------------------------------------------------------------

# Sets api, owner, the repositories' in-cluster URL base and credentials for
# $git_provider (gitea or github); for Gitea a port-forward until the script
# exits.
scm_connect() {
  case "$git_provider" in
    gitea)
      api="$gitea_url/api/v1"
      SCM_USER="$(secret_value "$ns" gitea-admin username)"
      SCM_PASS="$(secret_value "$ns" gitea-admin password)"
      owner="$SCM_USER"
      repo_base="$gitea_internal/$owner"
      push_base="$gitea_url/$owner"
      kubectl -n "$ns" port-forward svc/gitea-http "$gitea_port:3000" >/dev/null 2>&1 &
      scm_pf=$!
      for _ in $(seq 50); do curl -fs "$api/version" >/dev/null 2>&1 && break; sleep 0.2; done
      curl -fs "$api/version" >/dev/null || { echo "gitea: no answer on $gitea_url" >&2; return 1; }
      ;;
    github)
      api="https://api.github.com"
      if [[ -z ${GITHUB_TOKEN:-} && -n ${GITHUB_TOKEN_FILE:-} ]]; then
        GITHUB_TOKEN="$(tr -d '[:space:]' <"$GITHUB_TOKEN_FILE")"
      fi
      [[ -n ${GITHUB_TOKEN:-} ]] || { echo "github: set GITHUB_TOKEN or GITHUB_TOKEN_FILE (README)" >&2; return 1; }
      SCM_USER=x-access-token
      SCM_PASS="$GITHUB_TOKEN"
      owner="${GITHUB_OWNER:-$(scm "$api/user" | json 'd["login"]')}"
      repo_base="https://github.com/$owner"
      push_base="$repo_base"
      ;;
    *) echo "GIT_PROVIDER: gitea or github, not $git_provider" >&2; return 1 ;;
  esac
  export SCM_USER SCM_PASS
}

scm_disconnect() { [[ -n ${scm_pf:-} ]] && { kill "$scm_pf" 2>/dev/null; wait "$scm_pf" 2>/dev/null; } || true; }

# The provider's API; credentials go to curl on stdin, not on its command line.
scm() {
  if [[ $git_provider == gitea ]]; then
    printf 'user = "%s:%s"\n' "$SCM_USER" "$SCM_PASS"
  else
    printf 'header = "Authorization: Bearer %s"\nheader = "Accept: application/vnd.github+json"\n' "$SCM_PASS"
  fi | curl -fsS -K - -H 'Content-Type: application/json' "$@"
}

# git with the provider's credentials from the environment.
scm_git() {
  GIT_TERMINAL_PROMPT=0 git -c credential.helper= \
    -c credential.helper='!f() { echo "username=$SCM_USER"; echo "password=$SCM_PASS"; }; f' \
    -c user.name="litek8s platform" -c user.email=platform@lk.test "$@"
}

# Create the private repository $1 unless it exists.
scm_ensure_repo() {
  scm "$api/repos/$owner/$1" >/dev/null 2>&1 && return
  if [[ $git_provider == gitea ]]; then
    scm -X POST "$api/user/repos" -d "{\"name\":\"$1\",\"private\":true,\"default_branch\":\"main\"}" >/dev/null
  else
    gh repo create "$owner/$1" --private >/dev/null
  fi
}

# Whether repository $1 has no commits yet.
scm_repo_empty() {
  if [[ $git_provider == gitea ]]; then
    [[ $(scm "$api/repos/$owner/$1" | json 'd["empty"]') == True ]]
  else
    [[ $(scm "$api/repos/$owner/$1/branches" | json 'len(d)') == 0 ]]
  fi
}

# An Actions secret $2 (value on stdin) and variable $3=$4 for repository $1.
# GitHub encrypts secrets with the repository's key: gh does that.
scm_set_secret() {
  local value; value="$(cat)"
  if [[ $git_provider == gitea ]]; then
    scm -X PUT "$api/repos/$owner/$1/actions/secrets/$2" -d "{\"data\":\"$value\"}" >/dev/null
  else
    printf '%s' "$value" | gh secret set "$2" --repo "$owner/$1" >/dev/null
  fi
}
scm_set_variable() {
  if [[ $git_provider == gitea ]]; then
    scm -X POST "$api/repos/$owner/$1/actions/variables/$2" -d "{\"value\":\"$3\"}" >/dev/null 2>&1 ||
      scm -X PUT "$api/repos/$owner/$1/actions/variables/$2" -d "{\"name\":\"$2\",\"value\":\"$3\"}" >/dev/null
  else
    gh variable set "$2" --repo "$owner/$1" --body "$3" >/dev/null
  fi
}

# What up.sh chose, for smoke.sh: the ConfigMap argocd/platform.
platform_get() { kubectl -n argocd get configmap platform -o jsonpath="{.data.$1}" 2>/dev/null; }
