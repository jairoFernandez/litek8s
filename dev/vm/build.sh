#!/usr/bin/env bash
# Build the macOS dev VM's guest (static litek8s + kernel + initramfs) into
# the directory `litek8s dev up` boots from.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${LITEK8S_DEV_VM_ASSETS:-$HOME/Library/Application Support/dev.civojairo.litek8s/dev/vm}"
BIN="$("$HERE/build-guest-binary.sh")"
python3 "$HERE/build-guest.py" --litek8s "$BIN" --out "$OUT"
