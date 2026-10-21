#!/usr/bin/env bash
# End to end check of the registry configuration (docs/registries.md) on a
# dev cluster: a registry:2 behind TLS from a private CA with Basic auth (a
# hostNetwork pod on the node, filled by a skopeo Job). A pod pulling from it
# must fail without a registries.yaml (unknown CA), then with only the CA
# (no credentials), then with wrong credentials, and run once the file gives
# the CA and the right credentials. Each change is a `dev down` + `dev up`.
#
#   bench/registry-e2e.sh          run it (the cluster stays on failure)
#   bench/registry-e2e.sh down     delete the cluster and the test's files
#
# Runs this checkout's litek8s with dev/lk in its own LITEK8S_HOME (default
# ~/.litek8s-registry-e2e, so dev/lk's usual home and its clusters are left
# alone). On macOS the VM's guest must be this checkout's:
#   LITEK8S_HOME=~/.litek8s-registry-e2e dev/lk build-vm
# Env: E2E_CLUSTER (regtest), E2E_KEEP=1 keeps the cluster after a pass,
# E2E_PORT (5443, the registry's port on the node). Needs kubectl, openssl
# and htpasswd. The test owns $LITEK8S_HOME/registries.yaml.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
export LITEK8S_HOME="${LITEK8S_HOME:-$HOME/.litek8s-registry-e2e}"
NAME="${E2E_CLUSTER:-regtest}"
PORT="${E2E_PORT:-5443}"
LK="$ROOT/dev/lk"
WORK="$LITEK8S_HOME/registry-e2e"
REGISTRIES="$LITEK8S_HOME/registries.yaml"
KCFG="$LITEK8S_HOME/kubeconfig"
CTX="${LITEK8S_CONTEXT_PREFIX:-lkdev-}$NAME"
NS=regtest
USER_NAME=e2e PASSWORD=s3cret-e2e

log() { echo "[registry-e2e] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --kubeconfig "$KCFG" --context "$CTX" --request-timeout=20s "$@"; }

if [[ "${1:-}" == down ]]; then
  "$LK" dev down --name "$NAME" || true
  "$LK" dev delete --name "$NAME" || true
  rm -rf "$WORK" "$REGISTRIES"
  exit 0
fi

mkdir -p "$WORK"
rm -f "$REGISTRIES"

restart() {
  log "dev down + dev up ($1)"
  "$LK" dev down --name "$NAME" >/dev/null
  "$LK" dev up --name "$NAME" >/dev/null
}

"$LK" dev up --name "$NAME"
k create namespace "$NS" --dry-run=client -o yaml | k apply -f - >/dev/null
NODE_IP="$(k get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
[[ -n "$NODE_IP" ]] || fail "no node IP"
REG="$NODE_IP:$PORT"
IMAGE="$REG/e2e/busybox:1.36"
log "registry at $REG"

# A private CA and a serving certificate for the node's address.
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=registry-e2e CA" \
  -keyout "$WORK/ca.key" -out "$WORK/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=$NODE_IP" -keyout "$WORK/tls.key" -out "$WORK/tls.csr" 2>/dev/null
printf 'subjectAltName=IP:%s\nextendedKeyUsage=serverAuth\n' "$NODE_IP" >"$WORK/ext.cnf"
openssl x509 -req -in "$WORK/tls.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" -CAcreateserial -days 30 \
  -extfile "$WORK/ext.cnf" -out "$WORK/tls.crt" 2>/dev/null
htpasswd -Bbn "$USER_NAME" "$PASSWORD" >"$WORK/htpasswd"
k -n "$NS" create secret generic registry --from-file="$WORK/tls.crt" --from-file="$WORK/tls.key" \
  --from-file="$WORK/htpasswd" --dry-run=client -o yaml | k apply -f - >/dev/null

k apply -f - >/dev/null <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: registry, namespace: $NS}
spec:
  selector: {matchLabels: {app: registry}}
  strategy: {type: Recreate}
  template:
    metadata: {labels: {app: registry}}
    spec:
      hostNetwork: true
      containers:
        - name: registry
          image: registry:2
          env:
            - {name: REGISTRY_HTTP_ADDR, value: "0.0.0.0:$PORT"}
            - {name: REGISTRY_HTTP_TLS_CERTIFICATE, value: /certs/tls.crt}
            - {name: REGISTRY_HTTP_TLS_KEY, value: /certs/tls.key}
            - {name: REGISTRY_AUTH, value: htpasswd}
            - {name: REGISTRY_AUTH_HTPASSWD_REALM, value: registry-e2e}
            - {name: REGISTRY_AUTH_HTPASSWD_PATH, value: /certs/htpasswd}
          readinessProbe: {tcpSocket: {port: $PORT}, periodSeconds: 2}
          volumeMounts: [{name: certs, mountPath: /certs}]
      volumes: [{name: certs, secret: {secretName: registry}}]
EOF

# The registry keeps images in its container: fill it after every start.
seed() {
  k -n "$NS" rollout status deploy/registry --timeout=180s >/dev/null || fail "the registry did not start"
  k -n "$NS" delete job seed --ignore-not-found --wait=true >/dev/null
  k apply -f - >/dev/null <<EOF
apiVersion: batch/v1
kind: Job
metadata: {name: seed, namespace: $NS}
spec:
  backoffLimit: 6
  template:
    spec:
      restartPolicy: OnFailure
      containers:
        - name: copy
          image: quay.io/skopeo/stable:v1.16
          args: [copy, --dest-tls-verify=false, --dest-creds, "$USER_NAME:$PASSWORD",
                 docker://docker.io/library/busybox:1.36, docker://$IMAGE]
EOF
  k -n "$NS" wait --for=condition=complete job/seed --timeout=240s >/dev/null || {
    k -n "$NS" logs job/seed --tail=20 >&2 || true
    fail "could not copy busybox into the registry"
  }
}

# Start a pod of $IMAGE; wait until it runs or its pull fails, print
# `Running` or the pull error.
try_pull() {
  k -n "$NS" delete pod app --ignore-not-found --wait=true >/dev/null
  k -n "$NS" run app --image="$IMAGE" --image-pull-policy=Always --restart=Never -- sleep 3600 >/dev/null
  local msg=""
  for _ in $(seq 1 90); do
    if [[ "$(k -n "$NS" get pod app -o jsonpath='{.status.phase}')" == Running ]]; then
      echo Running
      return
    fi
    msg="$(k -n "$NS" get pod app -o jsonpath='{.status.containerStatuses[0].state.waiting.message}')"
    reason="$(k -n "$NS" get pod app -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}')"
    if [[ "$reason" == ErrImagePull || "$reason" == ImagePullBackOff ]] && [[ -n "$msg" ]]; then
      echo "$msg"
      return
    fi
    sleep 2
  done
  echo "timed out: ${msg:-no status}"
}

expect() {
  local what="$1" want="$2" got
  got="$(try_pull)"
  if [[ "$got" != *"$want"* ]]; then
    fail "$what: wanted \"$want\", got: $got"
  fi
  log "ok: $what -> ${got:0:160}"
}

seed
expect "no registries.yaml" "UnknownIssuer"

cat >"$REGISTRIES" <<EOF
configs:
  "$REG":
    tls:
      ca_file: registry-e2e/ca.pem
EOF
restart "the CA, no credentials"
seed
expect "the CA only" "requires authentication"

cat >>"$REGISTRIES" <<EOF
    auth:
      username: $USER_NAME
      password: wrong
EOF
restart "wrong credentials"
seed
expect "wrong credentials" "refused the credentials"

sed -i.bak "s/password: wrong/password: $PASSWORD/" "$REGISTRIES"
restart "the CA and the credentials"
seed
expect "the CA and the credentials" "Running"
[[ "$(k -n "$NS" exec app -- busybox | head -1)" == BusyBox* ]] || fail "the pod does not run the registry's busybox"

log "PASS"
if [[ "${E2E_KEEP:-}" != 1 ]]; then
  "$LK" dev down --name "$NAME" >/dev/null || true
  "$LK" dev delete --name "$NAME" >/dev/null || true
  rm -rf "$WORK" "$REGISTRIES"
fi
