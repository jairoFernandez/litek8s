#!/usr/bin/env bash
# The HA benchmark phases (bench/HA.md) against the cluster bench/ha.sh is
# running (litek8s or k3s). Results go to $OUT (default bench/results/ha-<kind>-<ts>).
#
#   bench/ha-run.sh latency          30 s of back-to-back writes through the LB
#   bench/ha-run.sh failover [kills] writes at 10/s while the leader is killed every 30 s
#   bench/ha-run.sh mem              PSS of every server process
#   bench/ha-run.sh corpus [every]   the compat corpus, killing the leader every <every> s (default 120)
#   bench/ha-run.sh conformance [every]  the conformance suite, same nemesis
#   bench/ha-run.sh lincheck [secs]  linearizability workload (litek8s) with kills and partitions
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
HA="$HERE/ha.sh"
WORK="${BENCH_WORK:-$HERE/.work}"
KIND="$(cat "$WORK/ha/kind" 2>/dev/null || echo litek8s)"
OUT="${OUT:-$HERE/results/ha-$KIND-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"
log() { echo "[ha-run $(date +%H:%M:%S)] $*" | tee -a "$OUT/run.log" >&2; }

# Start the nemesis (kill-leader every $1 s) in the background; prints its pid.
nemesis_start() {
  rm -f "$WORK/ha/nemesis.log"
  "$HA" nemesis "$1" 86400 >/dev/null 2>&1 &
  echo $!
}
nemesis_stop() { kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; cp "$WORK/ha/nemesis.log" "$OUT/$2-nemesis.log" 2>/dev/null || true; }

phase_latency() {
  log "latency: 30 s of sequential writes"
  "$HA" run python3 bench/node/haprobe.py run /home/bench/kubeconfig 30 0 /tmp/latency.jsonl
  "$VM_SH" ssh lk5 'cat /tmp/latency.jsonl' >"$OUT/latency.jsonl"
  python3 "$HERE/node/haprobe.py" report "$OUT/latency.jsonl" | tee "$OUT/latency.json"
}

phase_failover() {
  local kills="${1:-10}" secs pid
  secs=$(( kills * 30 + 20 ))
  log "failover: writes at 10/s for ${secs}s, the leader killed every 30 s"
  "$HA" run bash -c "nohup python3 bench/node/haprobe.py run /home/bench/kubeconfig $secs 100 /tmp/failover.jsonl >/dev/null 2>&1 &"
  pid=$(nemesis_start 30)
  sleep "$secs"
  nemesis_stop "$pid" failover
  sleep 5
  "$VM_SH" ssh lk5 'cat /tmp/failover.jsonl' >"$OUT/failover.jsonl"
  python3 "$HERE/node/haprobe.py" report "$OUT/failover.jsonl" "$OUT/failover-nemesis.log" | tee "$OUT/failover.json"
}

phase_mem() {
  log "memory"
  "$HA" mem | tee "$OUT/mem.txt"
}

phase_corpus() {
  local every="${1:-120}" pid
  log "corpus, killing the leader every ${every}s"
  pid=$(nemesis_start "$every")
  "$HA" run compat/run.sh --target "$KIND-ha-nemesis" >"$OUT/corpus.log" 2>&1 || true
  nemesis_stop "$pid" corpus
  grep -A12 '^| tier' "$OUT/corpus.log" | tee "$OUT/corpus-summary.md" || true
  grep -A20 'Not passing' "$OUT/corpus.log" >>"$OUT/corpus-summary.md" || true
}

phase_conformance() {
  local every="${1:-300}" pid
  log "conformance, killing the leader every ${every}s"
  pid=$(nemesis_start "$every")
  "$HA" run bash -c 'KUBECONFIG=$HOME/kubeconfig compat/conformance.sh --target '"$KIND"'-ha' >"$OUT/conformance.log" 2>&1 || true
  nemesis_stop "$pid" conformance
  "$VM_SH" ssh lk5 'cd ~/lk/compat/results && d=$(ls -td conformance-* | head -1) && tar -czf /tmp/conf.tgz "$d"' && "$VM_SH" ssh lk5 'cat /tmp/conf.tgz' >"$OUT/conformance-results.tgz"
  tail -30 "$OUT/conformance.log" | tee "$OUT/conformance-tail.txt"
}

phase_lincheck() {
  local secs="${1:-180}" pid i
  [[ "$KIND" == litek8s ]] || { log "lincheck needs litek8s (plain HTTP)"; return 0; }
  log "linearizability: ${secs}s of CRUD on 6 keys through every server, leader kills and partitions"
  "$HA" run bash -c "nohup litek8s ha-check --server 192.168.77.11:6443 --server 192.168.77.12:6443 --server 192.168.77.13:6443 --duration ${secs}s --clients 8 --keys 6 --out /tmp/history.json >/tmp/lincheck.out 2>&1 &"
  rm -f "$WORK/ha/nemesis.log"
  local end=$(( $(date +%s) + secs - 20 ))
  i=0
  while (( $(date +%s) < end )); do
    sleep 15
    if (( i % 2 == 0 )); then
      echo "$(date +%s.%N) kill $("$HA" kill-leader 2>/dev/null || echo none)" >>"$OUT/lincheck-nemesis.log"
    else
      local leader; leader="$("$HA" leader 2>/dev/null || true)"
      [[ -n "$leader" ]] && { echo "$(date +%s.%N) partition $leader" >>"$OUT/lincheck-nemesis.log"; "$HA" partition "$leader" 8; }
    fi
    i=$((i + 1))
  done
  # The check itself runs when the workload ends.
  while "$VM_SH" ssh lk5 'pgrep -f "litek8s ha-check" >/dev/null'; do sleep 5; done
  "$VM_SH" ssh lk5 'cat /tmp/lincheck.out' | tee "$OUT/lincheck.txt"
  "$VM_SH" ssh lk5 'gzip -c /tmp/history.json' >"$OUT/lincheck-history.json.gz"
}

VM_SH="$HERE/vm.sh"
case "${1:-}" in
  latency) phase_latency ;;
  failover) shift; phase_failover "$@" ;;
  mem) phase_mem ;;
  corpus) shift; phase_corpus "$@" ;;
  conformance) shift; phase_conformance "$@" ;;
  lincheck) shift; phase_lincheck "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
echo "$OUT"
