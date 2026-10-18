#!/usr/bin/env bash
# Copy this checkout's sources into a bench VM and build them there
# (natively, no Docker): bench/sync-vm.sh <vm> [cargo args...]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(cd "$HERE/.." && pwd)"
VM="$1"; shift
WORK="${BENCH_WORK:-$HERE/.work}"
port="$(cat "$WORK/vms/$VM/port")"
rsync -az --delete --exclude target --exclude .git --exclude bench/.work --exclude compat/.work --exclude compat/results \
  -e "ssh -i $WORK/id_ed25519 -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p $port" \
  "$ROOT/" bench@127.0.0.1:litek8s-src/
"$HERE/vm.sh" ssh "$VM" "cd litek8s-src && ~/.cargo/bin/cargo build --release ${*:-} 2>&1 | grep -E '^(error|warning: unused)' -A 12 | head -80; cp target/release/litek8s ~/litek8s 2>/dev/null; ls -la ~/litek8s"
