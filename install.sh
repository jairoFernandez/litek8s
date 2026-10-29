#!/bin/sh
# Install litek8s from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
#
# On a Linux server, also run it as a systemd service (like get.k3s.io):
#
#   curl -fsSL .../install.sh | sh -s -- server [apiserver flags...]
#   curl -fsSL .../install.sh | sh -s -- agent --join https://<server>:6443 --token <token>
#
# which installs the binary to /usr/local/bin (with sudo when not root),
# installs crun and iptables with the system's package manager when they
# are missing (LITEK8S_SKIP_DEPS=1 to skip), and runs `litek8s install
# server|agent ...`. LITEK8S_URL and LITEK8S_TOKEN stand for --join and
# --token.
#
# Downloads the archive for this OS and architecture (macOS arm64, Linux
# x86_64 or aarch64), checks it against the release's SHA256SUMS, and
# installs:
#   $LITEK8S_INSTALL_DIR/litek8s               (default ~/.local/bin)
#   <that dir>/../share/litek8s/vm/            (macOS: the dev VM's guest)
#
# Environment:
#   LITEK8S_VERSION      a tag such as v0.1.0 (default: the latest release)
#   LITEK8S_INSTALL_DIR  where the binary goes (default: ~/.local/bin;
#                        /usr/local/bin with server or agent)
#   LITEK8S_REPO         owner/name on GitHub (default: jairoFernandez/litek8s)
#   LITEK8S_DOWNLOAD_URL where the archives and SHA256SUMS are (default: the
#                        release's download URL on GitHub; for mirrors and tests)
set -eu

REPO="${LITEK8S_REPO:-jairoFernandez/litek8s}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "needs $1"; }

# `sh -s -- server|agent [flags...]`: also run litek8s as a service.
mode=""
case "${1:-}" in
  server | agent) mode="$1"; shift ;;
  "") ;;
  *) die "unknown mode $1 (use server or agent, or no argument to only install the binary)" ;;
esac
SUDO=""
if [ -n "$mode" ]; then
  [ "$(uname -s)" = Linux ] || die "\`$mode\` runs litek8s as a systemd service on Linux; on this machine use \`litek8s dev up\`"
  if [ "$(id -u)" != 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "run as root (or install sudo) to set up the $mode service"
    SUDO=sudo
  fi
  BIN_DIR="${LITEK8S_INSTALL_DIR:-/usr/local/bin}"
else
  BIN_DIR="${LITEK8S_INSTALL_DIR:-$HOME/.local/bin}"
fi

# crun runs the pods and iptables-restore sets up their network.
# shellcheck disable=SC2086 # $missing is a list of packages
install_deps() {
  missing=""
  command -v crun >/dev/null 2>&1 || [ -x /usr/bin/crun ] || [ -x /usr/local/bin/crun ] || missing="$missing crun"
  command -v iptables-restore >/dev/null 2>&1 || [ -x /usr/sbin/iptables-restore ] || [ -x /sbin/iptables-restore ] || missing="$missing iptables"
  [ -n "$missing" ] || return 0
  say "Installing$missing"
  if command -v apt-get >/dev/null 2>&1; then
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null &&
      $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $missing >/dev/null
  elif command -v dnf >/dev/null 2>&1; then
    $SUDO dnf install -y -q $missing
  elif command -v yum >/dev/null 2>&1; then
    $SUDO yum install -y -q $missing
  elif command -v zypper >/dev/null 2>&1; then
    $SUDO zypper --non-interactive install $missing
  elif command -v apk >/dev/null 2>&1; then
    $SUDO apk add $missing
  elif command -v pacman >/dev/null 2>&1; then
    $SUDO pacman -S --noconfirm --needed $missing
  else
    say "no known package manager: install$missing yourself"
    return 0
  fi || say "warning: could not install$missing (litek8s install will say what is missing)"
}

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

$SUDO mkdir -p "$BIN_DIR"
# Replace, not overwrite in place: a running litek8s (a dev VM, a server)
# keeps its old inode, and macOS keeps the new code signature valid.
$SUDO cp "$src/bin/litek8s" "$BIN_DIR/.litek8s.new"
$SUDO chmod 755 "$BIN_DIR/.litek8s.new"
$SUDO mv -f "$BIN_DIR/.litek8s.new" "$BIN_DIR/litek8s"
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

if [ -n "$mode" ]; then
  [ "${LITEK8S_SKIP_DEPS:-}" = 1 ] || install_deps
  if [ "$mode" = agent ] && [ -n "${LITEK8S_URL:-}" ]; then set -- --join "$LITEK8S_URL" "$@"; fi
  if [ -n "${LITEK8S_TOKEN:-}" ]; then set -- --token "$LITEK8S_TOKEN" "$@"; fi
  say ""
  exec $SUDO "$BIN_DIR/litek8s" install "$mode" "$@"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say ""; say "Add $BIN_DIR to your PATH, e.g.:"; say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile" ;;
esac

say ""
if [ "$os" = Linux ]; then
  say "Check the dev cluster's prerequisites (crun, uidmap, passt...):  litek8s dev setup"
  say "Install what is missing (and allow it under AppArmor):  sudo \"$BIN_DIR/litek8s\" dev setup"
fi
say "Start a local cluster:  litek8s dev up && kubectl get nodes"
