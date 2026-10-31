#!/usr/bin/env bash
# Compare the API server's speed of this checkout with a base commit, on this
# machine: bench/perf-baseline.json comes from another machine, so a pull
# request is measured against its base on the same runner, in the same job.
#
#   bench/perf-compare.sh [BASE_REF]      (default: main)
#
# The base is checked out in a git worktree and runs its own
# `cargo bench --bench api_load -- --save-baseline` into a temporary file
# (PERF_BASELINE); this checkout then runs against that file and fails on a
# regression beyond PERF_TOLERANCE. Runners are noisy, so a regression is
# measured again (base, then this checkout) and the job fails only when the
# same scenario regressed in every attempt.
# Both builds share CARGO_TARGET_DIR, so dependencies compile once.
#
# Env: PERF_TOLERANCE=0.20 PERF_ATTEMPTS=2, the api_load knobs (PERF_SECS,
# PERF_RUNS, PERF_CONC, ...), PERF_WORK=<dir> (default: a temporary one).
# Exit: 0 no regression (or nothing to compare with), 1 regression.
# In GitHub Actions the tables go to the job summary.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
BASE_REF="${1:-main}"
WORK="${PERF_WORK:-$(mktemp -d)}"
mkdir -p "$WORK"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export PERF_TOLERANCE="${PERF_TOLERANCE:-0.20}"
export PERF_BASELINE="$WORK/base.json"
ATTEMPTS="${PERF_ATTEMPTS:-2}"
BASE="$WORK/base"

summary() {
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] && printf '%s\n' "$@" >>"$GITHUB_STEP_SUMMARY"
  return 0
}

cleanup() { git -C "$ROOT" worktree remove --force "$BASE" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
git -C "$ROOT" worktree add --detach "$BASE" "$BASE_REF" >/dev/null
base_sha="$(git -C "$BASE" rev-parse --short HEAD)"
head_sha="$(git -C "$ROOT" rev-parse --short HEAD)"

# A base from before the comparison existed has nothing to measure.
if ! grep -q PERF_BASELINE "$BASE/benches/api_load.rs" 2>/dev/null; then
  echo "perf-compare: $BASE_REF ($base_sha) has no comparable benches/api_load.rs; nothing to compare"
  summary "### API server speed" "" "Base \`$base_sha\` has no comparable benchmark: nothing to compare."
  exit 0
fi

echo "perf-compare: building $BASE_REF ($base_sha) and this checkout ($head_sha)"
(cd "$BASE" && cargo bench --bench api_load --no-run)
(cd "$ROOT" && cargo bench --bench api_load --no-run)

summary "### API server speed: \`$head_sha\` vs base \`$base_sha\`" ""
for attempt in $(seq 1 "$ATTEMPTS"); do
  echo "perf-compare: attempt $attempt/$ATTEMPTS: base $base_sha"
  rm -f "$PERF_BASELINE"
  (cd "$BASE" && cargo bench --bench api_load -- --save-baseline) >"$WORK/base-$attempt.txt"
  echo "perf-compare: attempt $attempt/$ATTEMPTS: this checkout $head_sha"
  status=0
  PERF_REGRESSIONS="$WORK/regressed-$attempt" \
    bash -c 'cd "$1" && cargo bench --bench api_load' _ "$ROOT" >"$WORK/head-$attempt.txt" || status=$?
  cat "$WORK/head-$attempt.txt"
  summary "Attempt $attempt:" '```' "$(cat "$WORK/head-$attempt.txt")" '```'
  if [ ! -f "$WORK/regressed-$attempt" ]; then
    echo "perf-compare: api_load failed (exit $status) before comparing"
    exit 1
  fi
  # Only scenarios that regressed in every attempt so far still count.
  if [ "$attempt" -eq 1 ]; then
    sort -u "$WORK/regressed-1" >"$WORK/still"
  else
    sort -u "$WORK/regressed-$attempt" | comm -12 "$WORK/still" - >"$WORK/still.new"
    mv "$WORK/still.new" "$WORK/still"
  fi
  if [ ! -s "$WORK/still" ]; then
    echo "perf-compare: no scenario regressed in every attempt (tolerance $(awk "BEGIN{print $PERF_TOLERANCE*100}") %)"
    summary "No scenario regressed in every attempt."
    exit 0
  fi
  echo "perf-compare: attempt $attempt: still regressed: $(tr '\n' ' ' <"$WORK/still")"
done
summary "**Regressed in all $ATTEMPTS attempts: $(tr '\n' ' ' <"$WORK/still")**"
exit 1
