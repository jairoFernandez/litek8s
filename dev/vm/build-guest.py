#!/usr/bin/env python3
"""Build the macOS dev VM's guest: a Linux kernel, its root filesystem and a
small initramfs.

Everything comes from Alpine's arm64 packages, fetched and unpacked here (no
Docker, no root): the `linux-virt` kernel (its EFI zboot image unpacked to
the plain arm64 Image Virtualization.framework boots), the kernel modules the
cluster needs, and crun, iptables, iproute2, e2fsprogs and busybox with
their libraries. `/init` is litek8s itself (a static musl build, see
build-guest-binary.sh), which runs the cluster as PID 1.

    dev/vm/build-guest.py --litek8s target/guest-musl-arm64/release/litek8s --out <dir>

writes <dir>/Image, <dir>/root.img and <dir>/initrd.gz (what `litek8s dev
up` boots; it looks in $LITEK8S_DEV_VM_ASSETS, then <prefix>/share/litek8s/vm
next to an installed <prefix>/bin/litek8s, then ~/Library/Application
Support/dev.litek8s/vm/), and <dir>/PACKAGES.txt (every Alpine package in
it, with version, license and the aports commit of its source). The root filesystem is a read-only romfs image
the VM attaches as a disk, so the guest reads only the parts it runs and its
kernel can drop them again (an initramfs stays in RAM, whole, for good: ~55
MiB, which macOS charged the VM for all along); initrd.gz only loads the
modules for that disk and overlays a tmpfs on it for the few files init
writes (`--initramfs` builds the old single initramfs.gz instead).
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


class Romfs:
    """romfs image writer (Documentation/filesystems/romfs.rst): no
    compression, no owners (all root), modes 0644/0755; the simplest
    filesystem the kernel mounts read-only from a block device."""

    DIR, FILE, LINK, HARD = 1, 2, 3, 0

    @staticmethod
    def _pad(n):
        return (n + 15) & ~15

    @staticmethod
    def _checksum(data):
        """The word that makes the big-endian 32-bit words of data sum to 0."""
        total = sum(struct.unpack(f">{len(data) // 4}I", data)) & 0xFFFFFFFF
        return (-total) & 0xFFFFFFFF

    def __init__(self, root):
        self.root = root

    def _header_size(self, name):
        return 16 + self._pad(len(name.encode()) + 1)

    def build(self):
        # Layout: the superblock, then each directory's entries together
        # ("." and ".." first), directories breadth first.
        volname = b"litek8s"
        first = 16 + self._pad(len(volname) + 1)
        offset = first
        entries = []  # (offset, name, kind, path, dir_path)
        inodes = {}   # (dev, ino) -> offset of the first entry of that file
        dir_header = {self.root: first}  # dir path -> offset of its header
        dir_first = {}                   # dir path -> offset of its first entry
        queue = [(self.root, self.root)]
        while queue:
            path, parent = queue.pop(0)
            dir_first[path] = offset
            names = sorted(os.listdir(path))
            items = [(".", "dot", path), ("..", "dotdot", parent)] + [(n, None, os.path.join(path, n)) for n in names]
            for name, special, p in items:
                if special == "dot" and path == self.root:
                    kind = "rootdot"
                elif special:
                    kind = special
                else:
                    st = os.lstat(p)
                    if stat.S_ISLNK(st.st_mode):
                        kind = "link"
                    elif stat.S_ISDIR(st.st_mode):
                        kind = "dir"
                        dir_header[p] = offset
                        queue.append((p, path))
                    elif stat.S_ISREG(st.st_mode):
                        key = (st.st_dev, st.st_ino)
                        kind = ("hard", inodes[key]) if key in inodes else "file"
                        inodes.setdefault(key, offset)
                    else:
                        continue
                size = self._header_size(name)
                if kind == "file":
                    size += self._pad(os.path.getsize(p))
                elif kind == "link":
                    size += self._pad(len(os.readlink(p).encode()) + 1)
                entries.append((offset, name, kind, p, path))
                offset += size
        total = (offset + 1023) & ~1023
        out = bytearray(total)
        # Entries of one directory chain through `next`; the last has 0.
        by_dir = {}
        for e in entries:
            by_dir.setdefault(e[4], []).append(e)
        nxt = {}
        for lst in by_dir.values():
            for a, b in zip(lst, lst[1:] + [None]):
                nxt[a[0]] = b[0] if b else 0
        for off, name, kind, p, dirpath in entries:
            data = b""
            spec = 0
            mode = 0
            if kind == "rootdot":
                t, spec, mode = self.DIR, dir_first[dirpath], 8
            elif kind == "dot":
                t, spec = self.HARD, dir_header[dirpath]
            elif kind == "dotdot":
                t, spec = self.HARD, dir_header[p]
            elif kind == "dir":
                t, spec, mode = self.DIR, dir_first[p], 8
            elif kind == "link":
                t, data = self.LINK, os.readlink(p).encode() + b"\0"
            elif kind == "file":
                t = self.FILE
                with open(p, "rb") as f:
                    data = f.read()
                if os.lstat(p).st_mode & 0o111:
                    mode = 8
            else:
                t, spec = self.HARD, kind[1]
            nm = name.encode()
            nm = nm + b"\0" * (self._pad(len(nm) + 1) - len(nm))
            size = len(data) - 1 if t == self.LINK else len(data)
            hdr = bytearray(struct.pack(">IIII", nxt[off] | mode | t, spec, size, 0) + nm)
            struct.pack_into(">I", hdr, 12, self._checksum(bytes(hdr)))
            out[off:off + len(hdr)] = hdr
            out[off + len(hdr):off + len(hdr) + len(data)] = data
        sb = bytearray(b"-rom1fs-" + struct.pack(">II", total, 0) + volname + b"\0" * (first - 16 - len(volname)))
        out[0:len(sb)] = sb
        struct.pack_into(">I", out, 12, self._checksum(bytes(out[:min(512, total)])))
        return bytes(out)


# The initramfs's /init with a root disk: load what mounting it takes, put a
# tmpfs over it (init writes /etc/resolv.conf, creates the shares' mount
# points), and hand over to litek8s, which runs as PID 1 from the disk.
DISK_INIT = """#!/bin/busybox sh
bb=/bin/busybox
$bb mount -t devtmpfs devtmpfs /dev
$bb mount -t proc proc /proc
for m in /lib/modules/*.ko.gz; do $bb insmod "$m"; done
root=/dev/vdb
for w in $($bb cat /proc/cmdline); do case "$w" in litek8s.root=*) root="${w#litek8s.root=}";; esac; done
i=0
while [ ! -b "$root" ] && [ $i -lt 200 ]; do $bb usleep 10000; i=$((i+1)); done
$bb mkdir -p /lower /rw /root
if $bb mount -t romfs -o ro "$root" /lower; then
  $bb mount -t tmpfs -o mode=755 tmpfs /rw
  $bb mkdir -p /rw/upper /rw/work
  $bb mount -t overlay -o lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work overlay /root
fi
if [ ! -x /root/init ]; then
  echo "litek8s-guest: cannot mount the root disk $root" > /dev/console
  $bb sleep 5
  $bb poweroff -f
fi
$bb umount /proc
$bb umount /dev
exec $bb switch_root /root /init
"""


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
    ap.add_argument("--initramfs", action="store_true", help="the old layout: everything in one initramfs.gz")
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
    # What the guest is made of. The release archives ship these (mostly
    # GPL) binaries, so each one's source must be findable: the aports
    # commit and directory that build it.
    with open(os.path.join(out, "PACKAGES.txt"), "w") as f:
        f.write(f"# Alpine {args.release} {arch} packages in this guest: name version license source\n")
        for name in names:
            p = pkgs[name]
            f.write(f"{name} {p['V']} {p.get('L', '?')} "
                    f"https://gitlab.alpinelinux.org/alpine/aports/-/tree/{p.get('c', '?')}/{p['repo']}/{p.get('o', name)}\n")
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
    packages = " ".join(n for n in names if n != "linux-virt")
    if args.initramfs:
        cpio = Cpio()
        cpio.add_tree(root)
        data = cpio.finish()
        with gzip.open(os.path.join(out, "initramfs.gz"), "wb", compresslevel=6) as f:
            f.write(data)
        for stale in ["root.img", "initrd.gz"]:
            if os.path.exists(os.path.join(out, stale)):
                os.remove(os.path.join(out, stale))
        print(f"kernel {kernel_version}: {out}/Image ({os.path.getsize(os.path.join(out, 'Image')) >> 20} MiB), "
              f"{out}/initramfs.gz ({os.path.getsize(os.path.join(out, 'initramfs.gz')) >> 20} MiB, "
              f"{len(data) >> 20} MiB unpacked); packages: {packages}")
        return
    image = Romfs(root).build()
    with open(os.path.join(out, "root.img.part"), "wb") as f:
        f.write(image)
    os.rename(os.path.join(out, "root.img.part"), os.path.join(out, "root.img"))
    # The initramfs: busybox, the disk's modules, /init.
    small = os.path.join(build, "initrd")
    if os.path.exists(small):
        import shutil
        shutil.rmtree(small)
    for d in ["bin", "dev", "proc", "lib/modules", "lower", "rw", "root"]:
        os.makedirs(os.path.join(small, d), exist_ok=True)
    with open(os.path.join(root, "bin", bb), "rb") as a, open(os.path.join(small, "bin", "busybox"), "wb") as b:
        b.write(a.read())
    os.chmod(os.path.join(small, "bin", "busybox"), 0o755)
    for i, mod in enumerate(["kernel/drivers/block/virtio_blk.ko.gz", "kernel/fs/romfs/romfs.ko.gz", "kernel/fs/overlayfs/overlay.ko.gz"]):
        with open(os.path.join(src, mod), "rb") as a, open(os.path.join(small, "lib/modules", f"{i}-{os.path.basename(mod)}"), "wb") as b:
            b.write(a.read())
    with open(os.path.join(small, "init"), "w") as f:
        f.write(DISK_INIT)
    os.chmod(os.path.join(small, "init"), 0o755)
    cpio = Cpio()
    cpio.add_tree(small)
    with gzip.open(os.path.join(out, "initrd.gz"), "wb", compresslevel=9) as f:
        f.write(cpio.finish())
    if os.path.exists(os.path.join(out, "initramfs.gz")):
        os.remove(os.path.join(out, "initramfs.gz"))
    print(f"kernel {kernel_version}: {out}/Image ({os.path.getsize(os.path.join(out, 'Image')) >> 20} MiB), "
          f"{out}/root.img ({len(image) >> 20} MiB romfs), {out}/initrd.gz ({os.path.getsize(os.path.join(out, 'initrd.gz')) >> 10} KiB); "
          f"packages: {packages}")


if __name__ == "__main__":
    main()
