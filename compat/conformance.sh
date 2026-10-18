#!/usr/bin/env bash
# Run the official Kubernetes conformance suite (the upstream e2e.test binary,
# `[Conformance]` focus) from this host against the current kubeconfig.
#
#   compat/conformance.sh --target litek8s [--nodes 8] [--focus REGEX] [--skip REGEX]
#
# Parallel specs run first on --nodes ginkgo workers, then the [Serial] ones on
# one worker, like the upstream conformance image. Results (JUnit XML, the
# [Conformance] cases only, the gzipped ginkgo log, summary.md) go to
# compat/results/conformance-<ts>-<target>/.
# The test binaries are downloaded once into compat/.work/.
set -euo pipefail
dir=$(cd "$(dirname "$0")" && pwd)
version=${CONFORMANCE_VERSION:-v1.33.13}
target=cluster
nodes=8
focus='\[Conformance\]'
skip=''
serial=1
while [ $# -gt 0 ]; do
    case "$1" in
    --target) target=$2; shift 2 ;;
    --nodes) nodes=$2; shift 2 ;;
    --focus) focus=$2; shift 2 ;;
    --skip) skip=$2; shift 2 ;;
    --no-serial) serial=0; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

os=$(uname -s | tr A-Z a-z)
arch=$(uname -m); [ "$arch" = x86_64 ] && arch=amd64; [ "$arch" = aarch64 ] && arch=arm64
bin="$dir/.work/k8s-test-$version-$os-$arch/kubernetes/test/bin"
if [ ! -x "$bin/e2e.test" ]; then
    mkdir -p "$dir/.work/k8s-test-$version-$os-$arch"
    curl -fsSL "https://dl.k8s.io/$version/kubernetes-test-$os-$arch.tar.gz" |
        tar xz -C "$dir/.work/k8s-test-$version-$os-$arch"
    xattr -dr com.apple.quarantine "$bin" 2>/dev/null || true
fi

out="$dir/results/conformance-$(date +%Y%m%d-%H%M%S)-$target"
mkdir -p "$out"
kubeconfig=${KUBECONFIG:-$HOME/.kube/config}
e2e_args=(--kubeconfig="$kubeconfig" --provider=skeleton --disable-log-dump
    --dump-logs-on-failure=false)

run() { # <name> <ginkgo args...>
    local name=$1; shift
    mkdir -p "$out/$name"
    set +e
    "$bin/ginkgo" --timeout=24h --no-color --output-interceptor-mode=none "$@" \
        "$bin/e2e.test" -- "${e2e_args[@]}" --report-dir="$out/$name" \
        >"$out/$name/e2e.log" 2>&1
    echo "$name exit $?" >>"$out/exit-codes"
    set -e
}

start=$(date +%s)
parallel_skip='\[Serial\]'; [ -n "$skip" ] && parallel_skip="$parallel_skip|$skip"
run parallel --procs="$nodes" --focus="$focus" --skip="$parallel_skip"
if [ "$serial" = 1 ]; then
    serial_focus="\[Serial\].*$focus|$focus.*\[Serial\]"
    if [ -n "$skip" ]; then run serial --focus="$serial_focus" --skip="$skip"
    else run serial --focus="$serial_focus"; fi
fi
echo "$(( $(date +%s) - start ))" >"$out/seconds"
python3 "$dir/conformance_summary.py" "$out" "$target" >"$out/summary.md"
python3 "$dir/conformance_summary.py" --trim "$out"
cat "$out/summary.md" | head -30
echo "results: $out"
