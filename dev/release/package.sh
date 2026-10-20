#!/usr/bin/env bash
# Pack a release archive: dist/litek8s-<version>-<target>.tar.gz holding
#   litek8s-<version>-<target>/bin/litek8s
#   litek8s-<version>-<target>/share/litek8s/vm/   (with --vm: the macOS dev VM's guest)
#   litek8s-<version>-<target>/{LICENSE,NOTICE,THIRD-PARTY.md,README.md}
# The layout is what install.sh copies and what `litek8s dev up` looks for
# (src/dev/mod.rs vm_assets_dir): the guest in ../share/litek8s/vm from the binary.
#
# Usage: dev/release/package.sh <version> <target> <binary> [--vm <dir from build-guest.py>]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
version="$1" target="$2" binary="$3"
shift 3
vm=""
if [[ "${1:-}" == --vm ]]; then vm="$2"; fi
name="litek8s-$version-$target"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/bin"
cp "$binary" "$stage/$name/bin/litek8s"
chmod 755 "$stage/$name/bin/litek8s"
cp "$ROOT"/{LICENSE,NOTICE,THIRD-PARTY.md,README.md} "$stage/$name/"
if [[ -n "$vm" ]]; then
  mkdir -p "$stage/$name/share/litek8s/vm"
  for f in Image root.img initrd.gz PACKAGES.txt; do
    [[ -f "$vm/$f" ]] || { echo "package.sh: $vm/$f missing" >&2; exit 1; }
    cp "$vm/$f" "$stage/$name/share/litek8s/vm/"
  done
fi
mkdir -p "$ROOT/dist"
# Reproducible-ish: fixed owner, sorted entries where tar supports it.
if tar --version 2>/dev/null | grep -q GNU; then
  tar -C "$stage" --owner=0 --group=0 --sort=name -czf "$ROOT/dist/$name.tar.gz" "$name"
else
  tar -C "$stage" --uid 0 --gid 0 -czf "$ROOT/dist/$name.tar.gz" "$name"
fi
echo "$ROOT/dist/$name.tar.gz"
