#!/usr/bin/env bash
# Kubernetes compatibility corpus runner. Runs every case against the cluster in
# the current kubeconfig and reports a pass rate per tier and overall.
#
#   compat/run.sh                         # all tiers, current kubeconfig
#   compat/run.sh --tiers L0,L1           # only some tiers
#   compat/run.sh --only cm_create_get    # specific cases (comma-separated)
#   compat/run.sh --target k3d-baseline   # label used in the results dir
#   compat/run.sh --max-case-seconds 30   # cap every case's timeout (quick scoring)
#
# Results: compat/results/<timestamp>-<target>/{results.jsonl,summary.md,logs/}
set -uo pipefail
set -m # background jobs get their own process group, so a timeout kills kubectl too

COMPAT_DIR=$(cd "$(dirname "$0")" && pwd)
export COMPAT_DIR
source "$COMPAT_DIR/lib.sh"

tiers="L0,L1,L2,L3,L4"
only=""
max_case=0
target=$(kubectl config current-context 2>/dev/null || echo unknown)
while [[ $# -gt 0 ]]; do
    case $1 in
        --tiers) tiers=$2; shift 2 ;;
        --only) only=$2; shift 2 ;;
        --target) target=$2; shift 2 ;;
        --max-case-seconds) max_case=$2; shift 2 ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for f in "$COMPAT_DIR"/cases/*.sh; do
    tier=$(basename "$f" | cut -d- -f1)
    [[ ",$tiers," == *",$tier,"* ]] && source "$f"
done

out="$COMPAT_DIR/results/$(date +%Y%m%d-%H%M%S)-$target"
mkdir -p "$out/logs"
results="$out/results.jsonl"
: >"$results"

record() { # id tier status seconds detail
    python3 -c 'import json,sys; print(json.dumps(dict(id=sys.argv[1],tier=sys.argv[2],status=sys.argv[3],seconds=int(sys.argv[4]),detail=sys.argv[5])))' \
        "$@" >>"$results"
}

echo "target: $target   server: $(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}' 2>/dev/null)"
echo "results: $out"
for i in "${!COMPAT_CASES[@]}"; do
    id=${COMPAT_CASES[$i]}
    tier=${COMPAT_TIERS[$i]}
    timeout=${COMPAT_TIMEOUTS[$i]}
    ((max_case > 0 && timeout > max_case)) && timeout=$max_case
    if [[ -n "$only" && ",$only," != *",$id,"* ]]; then
        continue
    fi
    log="$out/logs/$id.log"
    start=$SECONDS
    ( set -eo pipefail; CASE_ID=$id; export CASE_ID; "t_$id" ) >"$log" 2>&1 &
    pid=$!
    status=""
    while kill -0 "$pid" 2>/dev/null; do
        if ((SECONDS - start >= timeout)); then
            kill -TERM -- "-$pid" 2>/dev/null
            sleep 1
            kill -KILL -- "-$pid" 2>/dev/null
            status=timeout
            break
        fi
        sleep 0.2
    done
    wait "$pid" 2>/dev/null
    rc=$?
    [[ -z "$status" ]] && { ((rc == 0)) && status=pass || status=fail; }
    elapsed=$((SECONDS - start))
    detail=$(grep -v '^[[:space:]]*$' "$log" | tail -1 | cut -c1-300)
    record "$id" "$tier" "$status" "$elapsed" "$detail"
    printf '%-4s %-26s %-7s %4ss  %s\n' "$tier" "$id" "$status" "$elapsed" \
        "$([[ $status == pass ]] || echo "$detail" | cut -c1-90)"
    # Per-case namespace cleanup; don't wait, deletion runs in the background.
    kubectl delete namespace "compat-${id//_/-}" --wait=false >/dev/null 2>&1 || true
done

python3 "$COMPAT_DIR/summary.py" "$out" "$target" >"$out/summary.md"
echo
cat "$out/summary.md"
