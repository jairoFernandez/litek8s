#!/bin/sh
# Install litek8s from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
#
# Downloads the archive for this OS and architecture (macOS arm64, Linux
# x86_64 or aarch64), checks it against the release's SHA256SUMS, and
# installs:
#   $LITEK8S_INSTALL_DIR/litek8s               (default ~/.local/bin)
#   <that dir>/../share/litek8s/vm/            (macOS: the dev VM's guest)
#
# Environment:
#   LITEK8S_VERSION      a tag such as v0.1.0 (default: the latest release)
#   LITEK8S_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
#   LITEK8S_REPO         owner/name on GitHub (default: jairoFernandez/litek8s)
#   LITEK8S_DOWNLOAD_URL where the archives and SHA256SUMS are (default: the
#                        release's download URL on GitHub; for mirrors and tests)
set -eu

REPO="${LITEK8S_REPO:-jairoFernandez/litek8s}"
BIN_DIR="${LITEK8S_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "needs $1"; }

need curl
need tar
need uname

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64) target="darwin-arm64" ;;
  Darwin/x86_64)
    # A shell under Rosetta on an Apple silicon Mac reports x86_64.
    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
      target="darwin-arm64"
    else
      die "Intel Macs are not supported (the dev VM is arm64 only)"
    fi ;;
  Linux/x86_64 | Linux/amd64) target="linux-x86_64" ;;
  Linux/aarch64 | Linux/arm64) target="linux-aarch64" ;;
  *) die "no release for $os/$arch; build from source: cargo install --git https://github.com/$REPO" ;;
esac

if [ -n "${LITEK8S_VERSION:-}" ]; then
  version="$LITEK8S_VERSION"
else
  # The /releases/latest page redirects to /releases/tag/<tag>; no API token
  # or rate limit involved.
  url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
    die "cannot reach github.com/$REPO (is the repository public, with a published release?)"
  version="${url##*/}"
  case "$version" in
    v*) ;;
    *) die "no published release found at github.com/$REPO/releases" ;;
  esac
fi

archive="litek8s-$version-$target.tar.gz"
base="${LITEK8S_DOWNLOAD_URL:-https://github.com/$REPO/releases/download/$version}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading litek8s $version for $target"
curl -fSL --progress-bar "$base/$archive" -o "$tmp/$archive" || die "download failed: $base/$archive"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "$archive is not in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')"
else
  die "needs sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive (expected $expected, got $actual)"

tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/litek8s-$version-$target"
[ -x "$src/bin/litek8s" ] || die "unexpected archive layout"

mkdir -p "$BIN_DIR"
# Replace, not overwrite in place: a running litek8s (a dev VM, a server)
# keeps its old inode, and macOS keeps the new code signature valid.
cp "$src/bin/litek8s" "$BIN_DIR/.litek8s.new"
chmod 755 "$BIN_DIR/.litek8s.new"
mv -f "$BIN_DIR/.litek8s.new" "$BIN_DIR/litek8s"
say "Installed $BIN_DIR/litek8s"

if [ -d "$src/share/litek8s/vm" ]; then
  share="$(cd "$BIN_DIR/.." && pwd)/share/litek8s"
  mkdir -p "$share"
  rm -rf "$share/vm.new"
  cp -R "$src/share/litek8s/vm" "$share/vm.new"
  rm -rf "$share/vm"
  mv "$share/vm.new" "$share/vm"
  say "Installed the dev VM guest in $share/vm"
fi

"$BIN_DIR/litek8s" --version >/dev/null 2>&1 || say "warning: $BIN_DIR/litek8s does not run on this machine"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say ""; say "Add $BIN_DIR to your PATH, e.g.:"; say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile" ;;
esac

say ""
if [ "$os" = Linux ]; then
  say "Check the dev cluster's prerequisites (crun, uidmap, passt...):  litek8s dev setup"
fi
say "Start a local cluster:  litek8s dev up && kubectl get nodes"
