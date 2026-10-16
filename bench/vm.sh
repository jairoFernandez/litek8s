#!/usr/bin/env bash
# Throwaway local Linux VMs for the benchmark: Ubuntu 24.04 arm64 cloud image
# under qemu+HVF, sized like an AWS t4g.micro (2 vCPU, 1 GiB, no swap).
# Every `up` starts from a fresh copy-on-write overlay, so runs never inherit
# state from a previous system under test.
#
#   bench/vm.sh up   <name> <ssh-port> [mem-MiB] [cpus]
#   bench/vm.sh ssh  <name> [cmd...]
#   bench/vm.sh scp  <name> <local> <remote>
#   bench/vm.sh down <name>
#
# To benchmark real machines instead (e.g. fresh EC2 t4g.micro from
# infra/terraform), export BENCH_HOST_<name>=user@host[:port] (and BENCH_KEY if
# not ~/.ssh/id_ed25519): `up` then attaches to that host instead of booting a
# VM, and `down` only forgets it. Hosts must be fresh Ubuntu 24.04, one per name.
#
# Requires: qemu-system-aarch64 (Homebrew), hdiutil (macOS) or genisoimage/xorriso.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
IMAGE_URL="https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-arm64.img"
IMAGE_SHA256="${BENCH_IMAGE_SHA256:-}"
BASE="$WORK/images/ubuntu-24.04-server-cloudimg-arm64.img"
KEY="$WORK/id_ed25519"
EXT_KEY="${BENCH_KEY:-$HOME/.ssh/id_ed25519}"
QEMU_SHARE="$(dirname "$(command -v qemu-system-aarch64)")/../share/qemu"

ensure_base() {
  mkdir -p "$WORK/images"
  if [[ ! -f "$BASE" ]]; then
    echo "downloading $IMAGE_URL" >&2
    curl -fL -o "$BASE.part" "$IMAGE_URL"
    mv "$BASE.part" "$BASE"
  fi
  if [[ -n "$IMAGE_SHA256" ]]; then
    echo "$IMAGE_SHA256  $BASE" | shasum -a 256 -c - >&2
  fi
  [[ -f "$KEY" ]] || ssh-keygen -q -t ed25519 -N '' -C litek8s-bench -f "$KEY"
}

ssh_opts() {
  local port="$1" key="${2:-$KEY}"
  echo "-i $key -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 -p $port"
}

port_of() { cat "$WORK/vms/$1/port"; }
# user@host for a VM name: an attached external host, or the local VM's forward.
target_of() { cat "$WORK/vms/$1/target" 2>/dev/null || echo bench@127.0.0.1; }
key_of() { [[ -f "$WORK/vms/$1/target" ]] && echo "$EXT_KEY" || echo "$KEY"; }

attach_external() { # name user@host[:port]
  local name="$1" spec="$2" dir="$WORK/vms/$1" port=22 target
  target="${spec%:*}"; [[ "$spec" == *:* ]] && port="${spec##*:}"
  rm -rf "$dir"; mkdir -p "$dir"
  echo "$target" >"$dir/target"; echo "$port" >"$dir/port"
  echo "waiting for ssh on external host $target:$port ($name)" >&2
  for _ in $(seq 1 60); do
    # shellcheck disable=SC2046
    ssh $(ssh_opts "$port" "$EXT_KEY") "$target" 'cloud-init status --wait >/dev/null 2>&1; true' 2>/dev/null && return 0
    sleep 5
  done
  echo "external host $target unreachable" >&2; return 1
}

make_seed() {
  local dir="$1" name="$2"
  mkdir -p "$dir/seed"
  cat >"$dir/seed/meta-data" <<EOF
instance-id: $name-$(date +%s)
local-hostname: $name
EOF
  cat >"$dir/seed/user-data" <<EOF
#cloud-config
users:
  - name: bench
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat "$KEY.pub")
ssh_pwauth: false
package_update: false
package_upgrade: false
# Keep the guest quiet so idle numbers measure the system under test, not
# Ubuntu housekeeping.
runcmd:
  - systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer man-db.timer motd-news.timer snapd.service snapd.socket snapd.seeded.service fwupd-refresh.timer 2>/dev/null || true
  - systemctl mask snapd.service snapd.socket 2>/dev/null || true
EOF
  rm -f "$dir/seed.iso"
  if command -v hdiutil >/dev/null; then
    hdiutil makehybrid -quiet -o "$dir/seed.iso" -iso -joliet -default-volume-name cidata "$dir/seed"
  elif command -v genisoimage >/dev/null; then
    genisoimage -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock "$dir/seed"
  else
    xorriso -as mkisofs -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock "$dir/seed"
  fi
}

cmd_up() {
  local name="$1" port="$2" mem="${3:-1024}" cpus="${4:-2}" ext_var="BENCH_HOST_$1"
  if [[ -n "${!ext_var:-}" ]]; then attach_external "$name" "${!ext_var}"; return; fi
  ensure_base
  local dir="$WORK/vms/$name"
  if [[ -f "$dir/qemu.pid" ]] && kill -0 "$(cat "$dir/qemu.pid")" 2>/dev/null; then
    echo "vm $name already running" >&2; return 0
  fi
  rm -rf "$dir"; mkdir -p "$dir"
  echo "$port" >"$dir/port"
  qemu-img create -q -f qcow2 -F qcow2 -b "$BASE" "$dir/disk.qcow2" 12G
  make_seed "$dir" "$name"
  cp "$QEMU_SHARE/edk2-aarch64-code.fd" "$dir/efi-code.fd"
  dd if=/dev/zero of="$dir/efi-vars.fd" bs=1m count=64 2>/dev/null
  truncate -s 64m "$dir/efi-code.fd"
  qemu-system-aarch64 \
    -name "$name" -machine virt,accel=hvf -cpu host -smp "$cpus" -m "$mem" \
    -drive if=pflash,format=raw,readonly=on,file="$dir/efi-code.fd" \
    -drive if=pflash,format=raw,file="$dir/efi-vars.fd" \
    -drive if=virtio,format=qcow2,file="$dir/disk.qcow2" \
    -drive if=virtio,format=raw,readonly=on,file="$dir/seed.iso" \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$port-:22" -device virtio-net-pci,netdev=n0 \
    -device virtio-rng-pci \
    -display none -serial "file:$dir/serial.log" \
    -daemonize -pidfile "$dir/qemu.pid"
  echo "waiting for ssh on $name (:$port)" >&2
  for _ in $(seq 1 120); do
    # shellcheck disable=SC2046
    if ssh $(ssh_opts "$port") bench@127.0.0.1 'cloud-init status --wait >/dev/null 2>&1; true' 2>/dev/null; then
      echo "vm $name up" >&2; return 0
    fi
    sleep 3
  done
  echo "vm $name did not come up; see $dir/serial.log" >&2; return 1
}

cmd_ssh() {
  local name="$1"; shift
  # shellcheck disable=SC2046
  ssh $(ssh_opts "$(port_of "$name")" "$(key_of "$name")") "$(target_of "$name")" "$@"
}

cmd_scp() {
  local name="$1" src="$2" dst="$3" port
  port="$(port_of "$name")"
  # shellcheck disable=SC2046
  scp -q $(ssh_opts "$port" "$(key_of "$name")" | sed 's/-p /-P /') "$src" "$(target_of "$name"):$dst"
}

cmd_down() {
  local dir="$WORK/vms/$1"
  # External hosts are never touched on down; only the attachment is forgotten.
  if [[ -f "$dir/qemu.pid" ]]; then
    kill "$(cat "$dir/qemu.pid")" 2>/dev/null || true
    sleep 1
  fi
  rm -rf "$dir"
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  ssh) shift; cmd_ssh "$@" ;;
  scp) shift; cmd_scp "$@" ;;
  down) shift; cmd_down "$@" ;;
  key) ensure_base; echo "$KEY" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
