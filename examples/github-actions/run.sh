#!/usr/bin/env bash
# Run the `run:` steps of an example workflow in order, from the example's
# directory, the way GitHub runs them (bash -e, one shell per step; steps
# with `if: always()` also after a failure, `if: failure()` only then).
# `uses:` steps are skipped: the caller sets up the cluster (this repo's CI
# runs the local setup-litek8s action, or kind for comparisons).
#
#   examples/github-actions/run.sh helm      # needs yq (on GitHub's runners)
#
# Prints one line per step with its duration, and STEP_SECONDS=<total>.
set -uo pipefail

ex=${1:?usage: run.sh <example>}
dir="$(cd "$(dirname "$0")/$ex" && pwd)"
wf="$dir/workflow.yml"
cd "$dir" || exit 2

job="$(yq '.jobs | keys | .[0]' "$wf")"
n="$(yq ".jobs.$job.steps | length" "$wf")"
failed=0
total_start=$(date +%s.%N)
for ((i = 0; i < n; i++)); do
    step=".jobs.$job.steps[$i]"
    run="$(yq "$step.run // \"\"" "$wf")"
    [[ -z "$run" ]] && continue
    name="$(yq "$step.name // \"step $i\"" "$wf")"
    cond="$(yq "$step.if // \"\"" "$wf")"
    case "$cond" in
        "") ((failed)) && continue ;;
        *always*) ;;
        *failure*) ((failed)) || continue ;;
        *) echo "run.sh: unsupported if: $cond" >&2; exit 2 ;;
    esac
    echo "::group::$name"
    start=$(date +%s.%N)
    bash -eo pipefail -c "$run"
    rc=$?
    secs=$(printf '%.1f' "$(echo "$(date +%s.%N) - $start" | bc)")
    echo "::endgroup::"
    if ((rc)); then
        echo "::error::step \"$name\" failed (exit $rc) after ${secs}s"
        failed=1
    else
        echo "step \"$name\": ${secs}s"
    fi
done
echo "STEP_SECONDS=$(printf '%.1f' "$(echo "$(date +%s.%N) - $total_start" | bc)")"
exit "$failed"
