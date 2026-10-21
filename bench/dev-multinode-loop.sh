#!/usr/bin/env bash
# The multi-node GitHub Actions example (examples/github-actions/multi-node)
# over and over on fresh `litek8s dev` clusters, the way CI starts them:
# `dev up --nodes 3`, the workload right away, then from every DaemonSet pod
# its resolv.conf and the `web` Service.
#
# Every pod (dnsPolicy ClusterFirst) must have the cluster DNS as its only
# nameserver, with the cluster search domains: a pod that also (or only)
# had the node's resolvers sent cluster names to the Internet (a public
# resolver answers `web` with 127.0.53.53). That check catches the startup
# race where a worker joined before the server's DNS was up, which the
# Service check (60 s of retries, as in the example) only sometimes did.
#
#   bench/dev-multinode-loop.sh [runs]        default 20
#
# Env: LITEK8S (binary, default litek8s on PATH), LITEK8S_HOME (use a
# scratch one: the cluster is created, and deleted at the end), LOOP_NAME
# (cluster name, default dnsloop), LOOP_NODES (3), LOOP_FRESH=1 (a new
# cluster every run instead of stopping and starting one: pulls the images
# every time; mind Docker Hub's anonymous limit of 100 pulls an hour, or
# put a mirror in $LITEK8S_HOME/registries.yaml, docs/registries.md). Prints one line per run and a
# summary; exits 1 if any run failed. Logs of failed runs stay in
# $LITEK8S_HOME/loop-<n>/.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
LK="${LITEK8S:-litek8s}"
NAME="${LOOP_NAME:-dnsloop}"
NODES="${LOOP_NODES:-3}"
RUNS="${1:-20}"
[[ -n "${LITEK8S_HOME:-}" ]] || { echo "set LITEK8S_HOME to a scratch directory" >&2; exit 2; }
export KUBECONFIG="$LITEK8S_HOME/loop.kubeconfig"
k() { kubectl --context "$ctx" --request-timeout=15s "$@"; }

run_once() { # n
  local n=$1 out="$LITEK8S_HOME/loop-$1" pod resolv ns
  mkdir -p "$out"
  "$LK" dev up --name "$NAME" --nodes "$NODES" --timeout 120 >"$out/up.log" 2>&1 || { echo "dev up failed"; return 1; }
  ctx="$("$LK" dev status --name "$NAME" -o json | python3 -c 'import json,sys; print(json.load(sys.stdin)["context"])')"
  echo "$ctx" >"$out/context"
  k apply -f "$ROOT/examples/github-actions/multi-node/spread.yaml" >/dev/null || { echo "apply failed"; return 1; }
  k rollout status deployment/web --timeout=3m >/dev/null && k rollout status daemonset/agent --timeout=3m >/dev/null || { echo "rollout failed"; return 1; }
  for pod in $(k get pods -l app=agent -o name); do
    resolv="$(k exec "$pod" -- cat /etc/resolv.conf)" || { echo "$pod: no resolv.conf"; return 1; }
    echo "$pod $(k get "$pod" -o jsonpath='{.spec.nodeName}'):" >>"$out/resolv.txt"
    echo "$resolv" >>"$out/resolv.txt"
    ns="$(grep -c '^nameserver' <<<"$resolv")"
    if [[ "$ns" != 1 ]] || ! grep -q '^search default.svc.cluster.local svc.cluster.local cluster.local' <<<"$resolv"; then
      echo "$pod on $(k get "$pod" -o jsonpath='{.spec.nodeName}'): not the cluster DNS alone: $(tr '\n' ';' <<<"$resolv")"
      return 1
    fi
    for i in $(seq 30); do
      k exec "$pod" -- wget -qO- -T 3 http://web >/dev/null 2>&1 && break
      [[ "$i" == 30 ]] && { echo "$pod cannot reach the web Service"; return 1; }
      sleep 2
    done
  done
}

pass=0 fail=0
for n in $(seq 1 "$RUNS"); do
  start=$SECONDS
  msg="$(run_once "$n")"
  rc=$?
  if ((rc == 0)); then
    pass=$((pass + 1)); echo "run $n: pass ($((SECONDS - start))s)"
    rm -rf "$LITEK8S_HOME/loop-$n"
  else
    fail=$((fail + 1)); echo "run $n: FAIL ($((SECONDS - start))s): $msg"
    "$LK" dev status --name "$NAME" >"$LITEK8S_HOME/loop-$n/status.txt" 2>&1 || true
    ctx="$(cat "$LITEK8S_HOME/loop-$n/context" 2>/dev/null)"
    {
      k get pods -o wide
      k get events --sort-by=.lastTimestamp
      for p in $(k get pods --field-selector=status.phase!=Running -o name); do
        k get "$p" -o jsonpath='{.metadata.name} {.spec.nodeName} {.status.containerStatuses}{"\n"}'
      done
    } >"$LITEK8S_HOME/loop-$n/pods.txt" 2>&1
    find "$LITEK8S_HOME" -path "*clusters/$NAME/dev.log" -exec cp {} "$LITEK8S_HOME/loop-$n/" \; 2>/dev/null || true
  fi
  if [[ "${LOOP_FRESH:-}" == 1 ]]; then
    # Deleting the last cluster stops the environment, which may close the
    # connection before it answers: delete until it is gone.
    for _ in 1 2 3 4 5; do
      "$LK" dev delete --name "$NAME" >/dev/null 2>&1
      "$LK" dev list 2>/dev/null | grep -q "^$NAME " || break
      sleep 2
    done
    if "$LK" dev list 2>/dev/null | grep -q "^$NAME "; then echo "cannot delete cluster $NAME"; exit 1; fi
  else
    # The same cluster stopped and started again: the same startup (server,
    # its DNS and local node, workers joining), and its images stay (each
    # fresh cluster would pull them again, against registry rate limits).
    ctx="$(cat "$LITEK8S_HOME/loop-$n/context" 2>/dev/null)"
    k delete -f "$ROOT/examples/github-actions/multi-node/spread.yaml" --wait=true --timeout=90s >/dev/null 2>&1 || true
    "$LK" dev down --name "$NAME" >/dev/null 2>&1 || true
  fi
done
[[ "${LOOP_FRESH:-}" == 1 ]] || "$LK" dev delete --name "$NAME" >/dev/null 2>&1 || true
echo "$pass passed, $fail failed of $RUNS"
((fail == 0))
