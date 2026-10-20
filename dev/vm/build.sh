#!/usr/bin/env bash
# Build the macOS dev VM's guest (static litek8s + kernel + initramfs) into
# the directory `litek8s dev up` boots from.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# The data directory litek8s uses (src/config.rs data_dir): dev.litek8s, or
# the pre-rename dev.civojairo.litek8s while only that one exists.
APP="$HOME/Library/Application Support"
DATA="$APP/dev.litek8s"
[[ ! -d "$DATA" && -d "$APP/dev.civojairo.litek8s" ]] && DATA="$APP/dev.civojairo.litek8s"
# With LITEK8S_HOME (dev/lk) its data directory is $LITEK8S_HOME/data.
[[ -n "${LITEK8S_HOME:-}" ]] && DATA="$LITEK8S_HOME/data"
OUT="${LITEK8S_DEV_VM_ASSETS:-$DATA/vm}"
BIN="$("$HERE/build-guest-binary.sh")"
python3 "$HERE/build-guest.py" --litek8s "$BIN" --out "$OUT"
