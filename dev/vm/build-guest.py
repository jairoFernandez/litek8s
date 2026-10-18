#!/usr/bin/env python3
"""Build the macOS dev VM's guest: a Linux kernel and an initramfs.

Everything comes from Alpine's arm64 packages, fetched and unpacked here (no
Docker, no root): the `linux-virt` kernel (its EFI zboot image unpacked to
the plain arm64 Image Virtualization.framework boots), the kernel modules the
cluster needs, and crun, iptables, iproute2, e2fsprogs and busybox with
their libraries. `/init` is litek8s itself (a static musl build, see
build-guest-binary.sh), which runs the cluster as PID 1.

    dev/vm/build-guest.py --litek8s target/guest-musl-arm64/release/litek8s --out <dir>

writes <dir>/Image and <dir>/initramfs.gz (what `litek8s dev up` boots; it
looks in ~/Library/Application Support/dev.civojairo.litek8s/dev/vm/ or
$LITEK8S_DEV_VM_ASSETS).
"""

import argparse
import gzip
import io
import os
import re
import stat
import struct
import sys
import tarfile
import urllib.request

MIRROR = "https://dl-cdn.alpinelinux.org/alpine"
PACKAGES = ["busybox-static", "crun", "iptables", "iproute2-minimal", "e2fsprogs"]
KERNEL = "linux-virt"  # only its kernel and modules are used, not its dependencies
# Kernel module directories to keep (everything else in linux-virt is
# hardware the VM does not have).
MODULE_DIRS = ["kernel/net", "kernel/fs", "kernel/drivers/net", "kernel/drivers/block", "kernel/drivers/virtio",
               "kernel/drivers/char", "kernel/lib", "kernel/crypto", "kernel/arch"]
SKIP_MODULE_DIRS = ["kernel/net/wireless", "kernel/net/bluetooth", "kernel/net/mac80211", "kernel/drivers/net/wireless",
                    "kernel/drivers/net/ethernet", "kernel/fs/nls", "kernel/fs/ocfs2", "kernel/fs/gfs2", "kernel/fs/xfs",
                    "kernel/fs/btrfs", "kernel/fs/nfs", "kernel/fs/smb", "kernel/fs/ceph", "kernel/fs/f2fs", "kernel/net/ceph"]

UDHCPC_SCRIPT = """#!/bin/sh
# Minimal udhcpc hook: configure eth0 from the lease.
case "$1" in
  deconfig) ip link set "$interface" up ;;
  bound|renew)
    # litek8s init configured the first lease; keep an unchanged address.
    if ! ip -4 addr show dev "$interface" | grep -q "inet $ip/"; then
      ip addr flush dev "$interface"
      ip addr add "$ip/${mask:-24}" dev "$interface"
    fi
    [ -n "$router" ] && ip route replace default via "${router%% *}" dev "$interface"
    : > /etc/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /etc/resolv.conf; done
    [ -n "$domain" ] && echo "search $domain" >> /etc/resolv.conf
    ;;
esac
exit 0
"""


def fetch(url, cache, name=None):
    path = os.path.join(cache, name or url.rsplit("/", 1)[1])
    if not os.path.exists(path):
        print(f"fetching {url}", file=sys.stderr)
        with urllib.request.urlopen(url) as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.rename(path + ".part", path)
    return path


def load_index(repo, arch, cache, release):
    path = fetch(f"{MIRROR}/{release}/{repo}/{arch}/APKINDEX.tar.gz", cache, f"APKINDEX-{release}-{repo}.tar.gz")
    with tarfile.open(path) as t:
        text = t.extractfile("APKINDEX").read().decode()
    pkgs = {}
    for block in text.split("\n\n"):
        fields = {}
        for line in block.splitlines():
            if len(line) > 2 and line[1] == ":":
                fields.setdefault(line[0], line[2:])
        if "P" in fields:
            fields["repo"] = repo
            pkgs[fields["P"]] = fields
    return pkgs


def resolve(names, pkgs):
    provides = {}
    for p in pkgs.values():
        provides.setdefault(p["P"], p["P"])
        for prov in p.get("p", "").split():
            provides.setdefault(re.split(r"[=<>~]", prov)[0], p["P"])
    out, todo = [], list(names)
    while todo:
        name = re.split(r"[=<>~]", todo.pop())[0]
        if name.startswith("!"):
            continue
        pkg = provides.get(name)
        if pkg is None:
            print(f"warning: nothing provides {name}", file=sys.stderr)
            continue
        if pkg in out:
            continue
        out.append(pkg)
        todo.extend(pkgs[pkg].get("D", "").split())
    return out


class Cpio:
    """newc archive writer; everything owned by root."""

    def __init__(self):
        self.buf = io.BytesIO()
        self.ino = 1
        self.seen = set()

    def _entry(self, name, mode, data=b"", nlink=1):
        name_b = name.encode() + b"\0"
        hdr = b"070701" + b"".join(b"%08x" % v for v in [
            self.ino, mode, 0, 0, nlink, 0, len(data), 0, 0, 0, 0, len(name_b), 0])
        self.ino += 1
        self.buf.write(hdr + name_b)
        self.buf.write(b"\0" * ((4 - (110 + len(name_b)) % 4) % 4))
        self.buf.write(data)
        self.buf.write(b"\0" * ((4 - len(data) % 4) % 4))

    def add_tree(self, root):
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames.sort()
            rel = os.path.relpath(dirpath, root)
            if rel != ".":
                self._entry(rel, stat.S_IFDIR | (os.lstat(dirpath).st_mode & 0o7777), nlink=2)
            for f in sorted(filenames) + [d for d in dirnames if os.path.islink(os.path.join(dirpath, d))]:
                p = os.path.join(dirpath, f)
                name = os.path.normpath(os.path.join(rel, f))
                st = os.lstat(p)
                if stat.S_ISLNK(st.st_mode):
                    self._entry(name, stat.S_IFLNK | 0o777, os.readlink(p).encode())
                elif stat.S_ISREG(st.st_mode):
                    with open(p, "rb") as fh:
                        self._entry(name, stat.S_IFREG | (st.st_mode & 0o7777), fh.read())

    def finish(self):
        self._entry("TRAILER!!!", 0)
        return self.buf.getvalue()


def unpack_apk(path, dest, keep):
    with gzip.open(path) as gz, tarfile.open(fileobj=gz, mode="r|") as t:
        for m in t:
            if m.name.startswith(".") and "/" not in m.name:
                continue  # .SIGN, .PKGINFO, scripts
            if not keep(m.name):
                continue
            target = os.path.join(dest, m.name)
            if m.isdir():
                os.makedirs(target, exist_ok=True)
                os.chmod(target, (m.mode & 0o7777) | 0o700)
            elif m.issym():
                os.makedirs(os.path.dirname(target), exist_ok=True)
                if os.path.lexists(target):
                    os.remove(target)
                os.symlink(m.linkname, target)
            elif m.islnk():
                os.makedirs(os.path.dirname(target), exist_ok=True)
                src = os.path.join(dest, m.linkname)
                if os.path.lexists(target):
                    os.remove(target)
                if os.path.exists(src):
                    os.link(src, target)
            elif m.isfile():
                os.makedirs(os.path.dirname(target), exist_ok=True)
                if os.path.lexists(target):
                    os.remove(target)
                with open(target, "wb") as f:
                    f.write(t.extractfile(m).read())
                os.chmod(target, (m.mode & 0o7777) | 0o600)


def unzboot(vmlinuz):
    """The arm64 Image inside an EFI zboot (gzip) kernel."""
    data = open(vmlinuz, "rb").read()
    if data[4:8] != b"zimg":
        return data
    off, size = struct.unpack_from("<II", data, 8)
    return gzip.decompress(data[off:off + size])


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--litek8s", required=True, help="static linux/arm64 litek8s binary (becomes /init)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--release", default="v3.22")
    ap.add_argument("--cache", default=None)
    args = ap.parse_args()
    out = os.path.abspath(args.out)
    cache = args.cache or os.path.join(out, "build", "cache")
    os.makedirs(cache, exist_ok=True)
    arch = "aarch64"
    pkgs = {}
    for repo in ["main", "community"]:
        for name, p in load_index(repo, arch, cache, args.release).items():
            pkgs.setdefault(name, p)
    names = resolve(PACKAGES, pkgs) + [KERNEL]
    build = os.path.join(out, "build")
    root = os.path.join(build, "rootfs")
    if os.path.exists(root):
        import shutil
        shutil.rmtree(root)
    os.makedirs(root)
    kernel_version = None
    for name in names:
        p = pkgs[name]
        apk = fetch(f"{MIRROR}/{args.release}/{p['repo']}/{arch}/{name}-{p['V']}.apk", cache)
        if name == "linux-virt":
            kdir = os.path.join(build, "kernel-pkg")
            unpack_apk(apk, kdir, lambda n: True)
            continue
        unpack_apk(apk, root, lambda n: not n.startswith(("usr/share/man", "usr/share/doc", "usr/include", "usr/share/bash-completion", "usr/share/zsh")))
    # Kernel and modules.
    open(os.path.join(out, "Image"), "wb").write(unzboot(os.path.join(kdir, "boot", "vmlinuz-virt")))
    moddir = os.path.join(kdir, "lib", "modules")
    kernel_version = os.listdir(moddir)[0]
    src = os.path.join(moddir, kernel_version)
    dst = os.path.join(root, "lib", "modules", kernel_version)
    for dirpath, _, filenames in os.walk(src):
        rel = os.path.relpath(dirpath, src)
        if rel != "." and not any(rel == d or rel.startswith(d + "/") for d in MODULE_DIRS):
            continue
        if any(rel == d or rel.startswith(d + "/") for d in SKIP_MODULE_DIRS):
            continue
        os.makedirs(os.path.join(dst, rel), exist_ok=True)
        for f in filenames:
            if rel == "." and not f.startswith("modules."):
                continue
            with open(os.path.join(dirpath, f), "rb") as a, open(os.path.join(dst, rel, f), "wb") as b:
                b.write(a.read())
    # Busybox applets, init and config.
    bb = "busybox.static" if os.path.exists(os.path.join(root, "bin", "busybox.static")) else "busybox"
    if bb != "busybox" and not os.path.lexists(os.path.join(root, "bin", "busybox")):
        os.symlink(bb, os.path.join(root, "bin", "busybox"))
    for applet in ["sh", "mount", "umount", "ls", "cat", "ps", "kill", "sleep", "mkdir", "rm", "cp", "mv", "ln", "echo",
                   "grep", "sed", "dmesg", "df", "du", "free", "top", "hostname", "nslookup", "wget", "vi", "less",
                   "tail", "head", "chmod", "chown", "sync", "uname", "env", "id", "true", "false", "test", "["]:
        link = os.path.join(root, "bin", applet)
        if not os.path.lexists(link):
            os.symlink(bb, link)
    os.makedirs(os.path.join(root, "sbin"), exist_ok=True)
    for applet in ["modprobe", "insmod", "lsmod", "udhcpc", "poweroff", "reboot"]:
        link = os.path.join(root, "sbin", applet)
        if not os.path.lexists(link):
            os.symlink("/bin/" + bb, link)
    for d in ["proc", "sys", "dev", "run", "tmp", "root", "var/lib/litek8s", "etc", "usr/share/udhcpc", "mnt"]:
        os.makedirs(os.path.join(root, d), exist_ok=True)
    os.chmod(os.path.join(root, "tmp"), 0o1777)
    with open(os.path.join(root, "usr/share/udhcpc/default.script"), "w") as f:
        f.write(UDHCPC_SCRIPT)
    os.chmod(os.path.join(root, "usr/share/udhcpc/default.script"), 0o755)
    for name, content in {
        "etc/passwd": "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n",
        "etc/group": "root:x:0:\ntty:x:5:\nnogroup:x:65534:\n",
        "etc/hosts": "127.0.0.1 localhost litek8s-dev\n::1 localhost\n",
        "etc/hostname": "litek8s-dev\n",
        "etc/resolv.conf": "",
        "etc/os-release": 'NAME="litek8s dev VM"\nID=litek8s\nPRETTY_NAME="litek8s dev VM (Alpine ' + args.release + ' userland)"\n',
    }.items():
        with open(os.path.join(root, name), "w") as f:
            f.write(content)
    init = os.path.join(root, "init")
    with open(args.litek8s, "rb") as a, open(init, "wb") as b:
        b.write(a.read())
    os.chmod(init, 0o755)
    os.symlink("/init", os.path.join(root, "bin", "litek8s"))
    cpio = Cpio()
    cpio.add_tree(root)
    data = cpio.finish()
    with gzip.open(os.path.join(out, "initramfs.gz"), "wb", compresslevel=6) as f:
        f.write(data)
    print(f"kernel {kernel_version}: {out}/Image ({os.path.getsize(os.path.join(out, 'Image')) >> 20} MiB), "
          f"{out}/initramfs.gz ({os.path.getsize(os.path.join(out, 'initramfs.gz')) >> 20} MiB, "
          f"{len(data) >> 20} MiB unpacked); packages: {' '.join(n for n in names if n != 'linux-virt')}")


if __name__ == "__main__":
    main()
