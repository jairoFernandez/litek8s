#!/usr/bin/env bash
# Relay choice by round trip, on a rootless `litek8s dev` cluster (Linux,
# no root needed; docs/networking.md#relays).
#
# Five nodes: the server (relay=false), worker and worker2 (WireGuard
# between them dropped with iptables in worker's cell: no direct path),
# worker3 and worker4 (relay=true). The pair must go through a relay; that
# relay is then slowed by 30 ms (netem on its cell's eth0), and the pair
# must move to the other one, both sides agreeing, with pods still reaching
# each other over the overlay.
#
#   LITEK8S_HOME=/tmp/lk-relay bench/relay-rtt.sh
#
# Env: LITEK8S (binary, default litek8s on PATH), LITEK8S_HOME (required:
# a scratch one; the cluster is created there and deleted at the end),
# RELAY_NAME (cluster name, default relayrtt), RELAY_DELAY_MS (30). Needs
# kubectl, python3, nsenter, iptables, tc and the sch_netem module. Exits 1
# when a step times out.
set -uo pipefail

LK="${LITEK8S:-litek8s}"
NAME="${RELAY_NAME:-relayrtt}"
DELAY="${RELAY_DELAY_MS:-30}"
[[ -n "${LITEK8S_HOME:-}" ]] || { echo "set LITEK8S_HOME to a scratch directory" >&2; exit 2; }
[[ "$(uname -s)" == Linux ]] || { echo "Linux only (cells are namespaces of this host)" >&2; exit 2; }
export KUBECONFIG="$LITEK8S_HOME/relay.kubeconfig"

log() { echo "[$(date +%T)] $*"; }
fail() { log "FAIL: $*"; exit 1; }
k() { kubectl --context "$ctx" --request-timeout=15s "$@"; }
# The agent of a worker node; its cell's namespaces, as the cell's root.
pid() { pgrep -f -- "--node-name $1 " | head -1; }
in_cell() { local p; p="$(pid "$1")"; shift; [[ -n "$p" ]] || fail "no agent process for $1"; nsenter -U -n -t "$p" "$@"; }
ip_of() { k get node "$1" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}'; }
routes() { k get node "$1" -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}'; }
route() { routes "$1" | python3 -c "import json,sys; print(json.load(sys.stdin).get('$2','-'))" 2>/dev/null; }
wait_for() { # seconds, description, condition (evaluated here)
  local t=$1 what=$2 cond=$3
  for _ in $(seq 1 "$t"); do eval "$cond" >/dev/null 2>&1 && { log "ok: $what"; return 0; }; sleep 1; done
  fail "timed out: $what"
}

cleanup() { "$LK" dev delete --name "$NAME" >/dev/null 2>&1; }
trap cleanup EXIT

"$LK" dev up --name "$NAME" --nodes 5 --timeout 120 | tail -1 || fail "dev up"
ctx="$("$LK" dev status --name "$NAME" -o json | python3 -c 'import json,sys; print(json.load(sys.stdin)["context"])')"
S="$(k get nodes -l node-role.kubernetes.io/control-plane -o jsonpath='{.items[0].metadata.name}')"
A="$S-worker"; B="$S-worker2"; R1="$S-worker3"; R2="$S-worker4"
k label node "$S" litek8s.dev/relay=false --overwrite >/dev/null
k label node "$R1" "$R2" litek8s.dev/relay=true --overwrite >/dev/null
wait_for 60 "5 nodes Ready" '[ "$(k get nodes --no-headers | grep -c " Ready")" = 5 ]'

IPB="$(ip_of "$B")"
log "dropping WireGuard between $A and $B ($IPB)"
in_cell "$A" iptables -I OUTPUT -d "$IPB" -p udp -j DROP
in_cell "$A" iptables -I INPUT -s "$IPB" -p udp -j DROP
wait_for 120 "pair relayed" 'case "$(route "$A" "$B")" in via*) true;; *) false;; esac'
# Round trips measured and reported on every link before slowing one.
wait_for 90 "round trips reported" '[ "$(k get node "$A" -o jsonpath="{.metadata.annotations.litek8s\.dev/wireguard-peers}" | grep -o rttMs | wc -l)" -ge 3 ]'
FIRST="$(route "$A" "$B")"; log "$A -> $B: $FIRST"
RELAY="${FIRST#via }"; OTHER="$R1"; [[ "$RELAY" == "$R1" ]] && OTHER="$R2"

log "slowing $RELAY by $DELAY ms"
in_cell "$RELAY" tc qdisc add dev eth0 root netem delay "${DELAY}ms" || fail "netem (is sch_netem available?)"
T0=$(date +%s)
wait_for 240 "pair moved to $OTHER" '[ "$(route "$A" "$B")" = "via $OTHER" ]'
log "moved after $(( $(date +%s) - T0 )) s"
[[ "$(route "$B" "$A")" == "via $OTHER" ]] || fail "$B goes $(route "$B" "$A"), $A via $OTHER"
log "both sides via $OTHER"

NET_B="$(k get node "$B" -o jsonpath='{.spec.podCIDR}')"
in_cell "$A" ping -c 3 -W 2 "${NET_B%.0/24}.1" | tail -1 || fail "no overlay traffic $A -> $B"
in_cell "$RELAY" tc qdisc del dev eth0 root
log "PASS"
