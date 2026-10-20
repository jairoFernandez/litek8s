#!/usr/bin/env bash
# Throwaway local Linux VMs for the benchmark: Ubuntu 24.04 arm64 cloud image
# under qemu+HVF, sized like an AWS t4g.micro (2 vCPU, 1 GiB, no swap).
# Every `up` starts from a fresh copy-on-write overlay, so runs never inherit
# state from a previous system under test.
#
#   bench/vm.sh up     <name> <ssh-port> [mem-MiB] [cpus] [lan-ip]
#   bench/vm.sh ssh    <name> [cmd...]
#   bench/vm.sh scp    <name> <local> <remote>
#   bench/vm.sh down   <name>
#   bench/vm.sh kill   <name>    power off hard (SIGKILL qemu), keep the disk
#   bench/vm.sh start  <name>    boot a killed VM again from its disk
#   bench/vm.sh pause  <name>    freeze the VM (a laptop going to sleep)
#   bench/vm.sh resume <name>
#
# With a lan-ip (e.g. 192.168.77.11) the VM gets a second NIC on a private
# L2 segment shared by every VM started with one (UDP datagrams to a small
# switch on loopback, bench/lanhub.py), so the VMs of a multi-node cluster
# reach each other directly. `bench/vm.sh lanhub-stop` stops the switch.
# BENCH_LAN_MCAST=1 (Linux hosts) uses a multicast group on loopback as the
# segment instead: the kernel forwards, which keeps up with bursts.
#
# To benchmark real machines instead (e.g. fresh EC2 t4g.micro from
# infra/terraform), export BENCH_HOST_<name>=user@host[:port] (and BENCH_KEY if
# not ~/.ssh/id_ed25519): `up` then attaches to that host instead of booting a
# VM, and `down` only forgets it. Hosts must be fresh Ubuntu 24.04, one per name.
#
# Requires: qemu-system-aarch64 (Homebrew), hdiutil (macOS) or genisoimage/xorriso.
# On an x86_64 Linux host (BENCH_VM_ARCH=amd64, the default there) the guests
# are amd64 Ubuntu under qemu-system-x86_64 with KVM instead (/dev/kvm).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${BENCH_WORK:-$HERE/.work}"
VM_ARCH="${BENCH_VM_ARCH:-$([[ "$(uname -m)" == x86_64 ]] && echo amd64 || echo arm64)}"
IMAGE_URL="https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-$VM_ARCH.img"
IMAGE_SHA256="${BENCH_IMAGE_SHA256:-}"
BASE="$WORK/images/ubuntu-24.04-server-cloudimg-$VM_ARCH.img"
KEY="$WORK/id_ed25519"
EXT_KEY="${BENCH_KEY:-$HOME/.ssh/id_ed25519}"

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

lan_mac() { # lan-ip -> a stable MAC from its last two octets
  local c d
  c="$(echo "$1" | cut -d. -f3)"; d="$(echo "$1" | cut -d. -f4)"
  printf '52:54:00:77:%02x:%02x' "$c" "$d"
}

# The switch every LAN NIC is plugged into (bench/lanhub.py), one per port.
ensure_lanhub() {
  local hub="$1" pidfile="$WORK/lanhub-$1.pid"
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then return 0; fi
  nohup python3 "$HERE/lanhub.py" "$hub" >/dev/null 2>&1 &
  echo $! >"$pidfile"
}

make_seed() {
  local dir="$1" name="$2" lan_ip="${3:-}"
  mkdir -p "$dir/seed"
  if [[ -n "$lan_ip" ]]; then
    cat >"$dir/seed/network-config" <<EOF
version: 2
ethernets:
  wan:
    match: {macaddress: "52:54:00:12:34:56"}
    dhcp4: true
  lan:
    match: {macaddress: "$(lan_mac "$lan_ip")"}
    addresses: [$lan_ip/24]
EOF
  fi
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
  local name="$1" port="$2" mem="${3:-1024}" cpus="${4:-2}" lan_ip="${5:-}" ext_var="BENCH_HOST_$1"
  if [[ -n "${!ext_var:-}" ]]; then attach_external "$name" "${!ext_var}"; return; fi
  ensure_base
  local dir="$WORK/vms/$name"
  if [[ -f "$dir/qemu.pid" ]] && kill -0 "$(cat "$dir/qemu.pid")" 2>/dev/null; then
    echo "vm $name already running" >&2; return 0
  fi
  rm -rf "$dir"; mkdir -p "$dir"
  echo "$port" >"$dir/port"
  echo "$mem $cpus $lan_ip" >"$dir/shape"
  qemu-img create -q -f qcow2 -F qcow2 -b "$BASE" "$dir/disk.qcow2" ${BENCH_DISK:-12G}
  make_seed "$dir" "$name" "$lan_ip"
  if [[ "$VM_ARCH" == arm64 ]]; then
    local share; share="$(dirname "$(command -v qemu-system-aarch64)")/../share/qemu"
    cp "$share/edk2-aarch64-code.fd" "$dir/efi-code.fd"
    dd if=/dev/zero of="$dir/efi-vars.fd" bs=1m count=64 2>/dev/null
    truncate -s 64m "$dir/efi-code.fd"
  fi
  boot "$name"
}

# Start qemu on the VM's existing disk and wait for ssh.
boot() {
  local name="$1" dir="$WORK/vms/$1" port mem cpus lan_ip lan=()
  port="$(port_of "$name")"
  read -r mem cpus lan_ip <"$dir/shape"
  if [[ -n "$lan_ip" ]]; then
    local hub="${BENCH_LAN_PORT:-47700}"
    if [[ -n "${BENCH_LAN_MCAST:-}" ]]; then
      # Linux: one multicast group on loopback is the segment; the kernel
      # forwards, no userspace switch to saturate.
      lan=(-netdev "socket,id=n1,mcast=230.77.0.1:$hub,localaddr=127.0.0.1"
           -device "virtio-net-pci,netdev=n1,mac=$(lan_mac "$lan_ip")")
    else
      ensure_lanhub "$hub"
      lan=(-netdev "dgram,id=n1,local.type=inet,local.host=127.0.0.1,local.port=$((hub + ${lan_ip##*.})),remote.type=inet,remote.host=127.0.0.1,remote.port=$hub"
           -device "virtio-net-pci,netdev=n1,mac=$(lan_mac "$lan_ip")")
    fi
  fi
  local machine=()
  if [[ "$VM_ARCH" == arm64 ]]; then
    machine=(qemu-system-aarch64 -machine virt,accel=hvf -cpu host
             -drive "if=pflash,format=raw,readonly=on,file=$dir/efi-code.fd"
             -drive "if=pflash,format=raw,file=$dir/efi-vars.fd")
  else
    # The cloud image boots with SeaBIOS too.
    machine=(qemu-system-x86_64 -machine q35,accel=kvm -cpu host)
  fi
  "${machine[@]}" \
    -name "$name" -smp "$cpus" -m "$mem" \
    -drive if=virtio,format=qcow2,file="$dir/disk.qcow2" \
    -drive if=virtio,format=raw,readonly=on,file="$dir/seed.iso" \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$port-:22" -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
    ${lan[@]+"${lan[@]}"} \
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

qemu_pid() { cat "$WORK/vms/$1/qemu.pid" 2>/dev/null || true; }

cmd_kill() {
  local pid; pid="$(qemu_pid "$1")"
  if [[ -n "$pid" ]]; then kill -9 "$pid" 2>/dev/null || true; fi
  rm -f "$WORK/vms/$1/qemu.pid"
}

cmd_start() {
  local pid; pid="$(qemu_pid "$1")"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then echo "vm $1 already running" >&2; return 0; fi
  boot "$1"
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
  kill) shift; cmd_kill "$@" ;;
  start) shift; cmd_start "$@" ;;
  pause) kill -STOP "$(qemu_pid "$2")" ;;
  resume) kill -CONT "$(qemu_pid "$2")" ;;
  lanhub-stop) for f in "$WORK"/lanhub-*.pid; do [[ -f "$f" ]] && { kill "$(cat "$f")" 2>/dev/null || true; rm -f "$f"; }; done ;;
  key) ensure_base; echo "$KEY" ;;
  *) sed -n '2,24p' "$0"; exit 2 ;;
esac
