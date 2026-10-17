# Shared helpers for the compatibility corpus. Sourced by run.sh and by every
# file under cases/. Cases are plain bash functions named `t_<id>`; each one is
# registered with `case_def` and runs in its own subshell with `set -e`, so the
# first failing command fails the case.

# Parallel indexed arrays (macOS ships bash 3.2, which has no associative ones).
COMPAT_CASES=()
COMPAT_TIERS=()
COMPAT_TIMEOUTS=()
COMPAT_DESCS=()

# case_def <id> <tier> <timeout-seconds> <description>
case_def() {
    COMPAT_CASES+=("$1")
    COMPAT_TIERS+=("$2")
    COMPAT_TIMEOUTS+=("$3")
    COMPAT_DESCS+=("$4")
}

# Run a command with a hard timeout. macOS ships no `timeout`, so use perl's
# alarm; the exec keeps the PID, so the alarm kills the command itself.
with_timeout() {
    local secs=$1
    shift
    perl -e 'alarm shift; exec @ARGV or die "exec failed: $!"' "$secs" "$@"
}

k() {
    with_timeout 60 kubectl "$@"
}

# Create (or reuse) a namespace dedicated to the current case and export it as
# $NS. The runner deletes it after the case, pass or fail.
use_ns() {
    NS="compat-${CASE_ID//_/-}"
    k create namespace "$NS" >/dev/null 2>&1 || k get namespace "$NS" >/dev/null
    export NS
}

# Poll `"$@"` once a second until it succeeds or <secs> elapse.
wait_for() {
    local secs=$1
    shift
    local deadline=$((SECONDS + secs))
    until "$@" >/dev/null 2>&1; do
        if ((SECONDS >= deadline)); then
            echo "timed out after ${secs}s waiting for: $*" >&2
            return 1
        fi
        sleep 1
    done
}

expect_eq() {
    if [[ "$1" != "$2" ]]; then
        echo "expected '$2', got '$1'" >&2
        return 1
    fi
}

expect_contains() {
    if [[ "$1" != *"$2"* ]]; then
        echo "expected output to contain '$2', got: $1" >&2
        return 1
    fi
}

# Number of schedulable nodes in the target cluster.
node_count() {
    k get nodes --no-headers 2>/dev/null | wc -l | tr -d ' '
}

# A throwaway client pod used by network cases. Prints nothing; waits Ready.
client_pod() {
    k -n "$NS" run client --image=busybox:1.36 --restart=Never -- sleep 3600 >/dev/null
    k -n "$NS" wait --for=condition=Ready pod/client --timeout=120s >/dev/null
}
