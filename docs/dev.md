# `litek8s dev`: a local Kubernetes cluster for development

`litek8s dev` is a local Kubernetes cluster for your laptop, in the role
kind, k3d, minikube or Docker Desktop's Kubernetes play, without Docker; as
many as you like, each with as many nodes as you like, all in one small VM
(macOS) or one rootless sandbox (Linux).
It is the same `litek8s apiserver` (API server, controllers, scheduler,
cluster DNS, service proxy) with a node agent on crun, packaged so that:

```bash
litek8s dev up          # ~0.1 s on Linux, ~1.5 s on macOS; merges context litek8s-dev into ~/.kube/config
kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')
litek8s dev load myapp:dev       # an image of your local Docker/Podman, or a `docker save` archive
litek8s dev status               # mode, server, pods, memory, disk
litek8s dev down                 # stop; objects and images are kept
litek8s dev delete               # stop and delete everything, and the kube context (`reset` does the same)
litek8s dev up --name ci --nodes 3   # another cluster: context litek8s-ci, 3 nodes
litek8s dev list                 # every local cluster: state, API port, nodes, memory, age
```

Numbers against kind and k3d are in [bench/RESULTS.md](../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d);
the corpus score in [compat/RESULTS.md](../compat/RESULTS.md).

## Linux: a rootless sandbox

The cluster runs in namespaces of its own, as your user:

| namespace | what it isolates |
|---|---|
| user | your uid keeps its number inside; your `/etc/subuid` range backs every other id, root included, so image layers keep their owners and files a container creates in a bind mount as your uid are yours on the host (`LITEK8S_DEV_IDMAP=root` makes you root inside instead, Docker-rootless style) |
| network | the `litek8s0` bridge, pod veths and **every** iptables rule (pod NAT, Service DNAT) |
| mount | a fresh `/run` (pod network namespaces, crun state), sysfs, a cgroup2 mount rooted at the cluster's cgroup |
| PID | PID 1 is a small init; stopping it stops every pod process |
| cgroup, UTS, IPC | the node's cgroup tree, hostname `litek8s-dev` |

Pods reach the Internet through [pasta](https://passt.top) (or
slirp4netns), a userspace network stack on the host side, the way rootless
Podman does; `LITEK8S_DEV_NETWORK=slirp4netns` picks slirp4netns where both
are installed (what [CI](ci.md#pasta-slirp4netns-and-githubs-runners) uses). The host side (`litek8s dev supervise`) binds the API port on
`127.0.0.1` before the sandbox exists, and binds NodePorts and LoadBalancer
ports on `127.0.0.1` when the cluster asks for them (it receives the
listening sockets as file descriptors), so `localhost:<nodePort>` works
without kind's `extraPortMappings`.

**What it never touches:** the host's network interfaces, routes and
iptables/nftables rules (Docker's or anyone's), `/run`, other users' files.
Everything it writes is under `~/.local/share/litek8s/dev`.

### What it needs

| need | why | how |
|---|---|---|
| `crun` | runs the containers | `apt install crun` / `dnf install crun` |
| `newuidmap`, `newgidmap` (setuid) | map your subordinate ids into the user namespace | package `uidmap` (Debian/Ubuntu) or `shadow-utils`; usually already installed |
| a subuid/subgid range of 65536 ids | owners inside images | `/etc/subuid`, `/etc/subgid`; `useradd` creates them on current distros |
| `pasta` or `slirp4netns` | pod egress | package `passt` (or `slirp4netns`) |
| `ip`, `iptables-restore` | used inside the sandbox only | `iproute2`, `iptables` (nft or legacy backend) |
| cgroup v2 + a systemd user session | a delegated cgroup for the cluster's resource limits | `dev up` starts the supervisor in a `systemd-run --user --scope -p Delegate=yes` scope |
| Linux ≥ 5.11 | overlayfs in a user namespace (`userxattr`) | |
| **Ubuntu ≥ 23.10 only:** `sudo litek8s dev setup`, once | AppArmor restricts unprivileged user namespaces; this installs `/etc/apparmor.d/litek8s` (an unconfined profile with `userns`, like Ubuntu's own profile for crun) for the litek8s binary's path | re-run if you move the binary |

`litek8s dev setup` (without sudo) checks all of this and says what is
missing. That is the whole list: no daemon, no group membership, no
capabilities on the binary, no setuid litek8s. Run as root, `dev up` uses the
same sandbox minus the user namespace (cgroup `/sys/fs/cgroup/litek8s-dev`).

## macOS: a small VM on Virtualization.framework

`dev up` boots a Linux VM with Apple's Virtualization.framework (the
`litek8s dev-vm` process; no Docker Desktop, OrbStack, Lima or vfkit). The
guest is Alpine's `linux-virt` kernel and a read-only root disk (a romfs
image of crun, iptables, iproute2, e2fsprogs and busybox around a static
litek8s that runs as PID 1; a 0.75 MiB initrd mounts it under a tmpfs
overlay and hands over); a sparse disk holds images and objects. The Mac
side:

- forwards `127.0.0.1:<port>` to the API server (6443 when free);
- watches the Services and listens on `127.0.0.1:<nodePort>` for every
  NodePort and on `127.0.0.1:<port>` for LoadBalancer ports
  (`EXTERNAL-IP` is `127.0.0.1`, as with Docker Desktop's Kubernetes);
- shares `$HOME` (or `--share <dir>`) over virtiofs at the **same path**, so
  `hostPath: /Users/you/project` volumes work and edits show up live (also
  `/private/tmp` and `/private/var/folders`, where `/tmp` and `$TMPDIR` are);
- passes file changes under bind-mounted directories into the VM (FSEvents
  on the Mac, IN_ATTRIB in the guest), so inotify watchers see Mac edits;
- shares Rosetta when it is installed, so `linux/amd64` images run;
- serves the Docker API on `~/.litek8s/docker.sock` (see [docker.md](docker.md));
- uses the console as the control channel (the guest reports its address
  and its memory, `dev down` powers it off cleanly);
- keeps the guest on as little of its memory as it needs, growing it on
  demand ([Memory](#memory-on-macos)).

The framework requires the `com.apple.security.virtualization` entitlement.
Release binaries are ad-hoc signed with it; a binary that lacks it (a
`cargo build`), makes `dev up` run an ad-hoc signed copy
(`codesign --sign -`), so nothing needs `sudo` or an Apple developer
account. VM size: `--memory` (MiB,
default 4096, an upper bound the guest grows into, see below) and `--cpus`
(default half the Mac's).

The guest (kernel, root image, initrd) comes with the macOS release
archive, in `share/litek8s/vm` beside `bin/litek8s` (install.sh puts it in
`~/.local/share/litek8s/vm`); `dev up` looks there first, so a release
binary always boots the guest of the same release. From source, build the
guest once (needs a Docker API or any Linux box for the musl build; a running `litek8s dev` serves one, so litek8s can build its next
guest itself, see [develop-without-docker.md](develop-without-docker.md)):

```bash
dev/vm/build.sh   # static litek8s, then kernel + root image + initrd from Alpine packages
```

### Memory on macOS

Virtualization.framework never gives a VM's memory back to macOS: every
page the guest has touched once stays charged to the VM's process (its
footprint, what Activity Monitor shows) until the VM stops, however much
the guest frees. Dropping 1.5 GB of guest page cache moved the footprint by
nothing; its balloon device does not help either (it offers no free page
reporting, and pages handed to it stay charged: inflating it by 3 GiB moved
the footprint by 0 MB, or by +540 MB with Alpine's `init_on_alloc`), and
saving and restoring the VM brings all of its memory back in (4.2 GB
footprint for a guest using 300 MB). Only stopping the VM returns memory;
OrbStack and Docker Desktop got dynamic memory by leaving the framework for
their own VMMs on Hypervisor.framework.

What litek8s does instead is keep the guest off memory it does not need, so
there is less to charge:

- **A ceiling that grows on demand.** The VM boots with 1 GiB (a quarter
  of `--memory` if that is more) for the guest; the balloon takes the rest
  before the guest has touched it, which costs nothing (the guest kernel
  runs with `init_on_alloc=0`, so it does not zero what it hands over).
  The guest reports its memory on the console every 2 s when it changes
  (every 250 ms when little is left) and at once when a PSI trigger fires
  (memory stalls of 100 ms in a second); `litek8s dev-vm` grows the ceiling
  by half (at least 512 MiB) when MemAvailable drops under 256 MiB (1/16
  of the VM) or memory stalls while free memory is short (stalls with
  plenty free come from elsewhere, a pod at its own memory limit for
  one), and to the whole VM when it is nearly out.
  It shrinks again, by half the excess at a time, once the guest has been
  calm for 2 minutes with more than twice that available: page cache then
  stays within memory the guest already has instead of spreading over all
  of it (an image pull no longer leaves the VM charged for gigabytes), and
  the guest drops its oldest cache first (MGLRU); hot cache shows up as
  stalls and grows the ceiling back.
- **No livelock under the ceiling.** With the root filesystem on a disk,
  the code everything runs is reclaimable page cache; a burst of
  allocations used to make the guest evict and re-read it for a minute
  (the node agent lost its connection) instead of running out of memory.
  The guest protects its working set of the last second
  (`lru_gen/min_ttl_ms=1000`), so the kernel takes its OOM path at once,
  and there the balloon deflates (the framework's balloon may deflate on
  OOM) before anything is killed; the host grows the ceiling meanwhile.
- **A smaller fixed cost.** The root filesystem is a disk image the guest
  reads as it needs it (an initramfs is unpacked into RAM and stays there:
  56 MiB, plus the 25 MiB archive the framework loads at boot), and the
  kernel skips its 64 MiB of bounce buffers (`swiotlb=noforce`: virtio
  devices here reach all memory).

`LITEK8S_DEV_DYNAMIC_MEMORY=0 litek8s dev up` gives the guest all of
`--memory` from the start. The ceiling and what moved it are in the VM's
log (`litek8s dev-vm: memory ceiling 1024 -> 1536 MiB (little available;
...)`, `env.log` in the dev data directory), and `litek8s ui` shows it with
the memory breakdown. To hand back memory the VM once used (after a big
build, say), restart it: `litek8s dev down && litek8s dev up` (objects,
images and volumes are kept; pods restart).

What macOS charges the VM (`--cpus 4 --memory 4096`, fresh cluster each
time, `bench/vm-memory.sh`; before: the guest from before these changes,
an initramfs and no ceiling):

| | before | now |
|---|---|---|
| idle, nothing deployed | 384 MB | 295 MB |
| idle, a pod | 461 MB | 389 MB |
| Argo CD installed (7 pods) | 1847 MB | 1140 MB (the ceiling stayed at 1 GiB) |
| then a 1 GB image pulled | 3296 MB | 1200 MB |
| then 5 minutes idle | 3296 MB | 1210 MB |
| then 1 GiB allocated at once in a pod | 0.39 s, 4083 MB | 0.36 s, 1596 MB (ceiling 1 → 4 GiB); no OOM kill either way |
| 1 GiB written, read and deleted in a container | +1058 MB, for good | +733 MB, for good |

After the 1 GiB burst the ceiling came back down within 4 minutes (4096 →
2471 → 1662 → 1257 → 1052 MiB) while a 600 MB file read every 10 s stayed
in the guest's cache throughout (125 ms per read, no refaults). For
comparison, OrbStack's VM (its own VMM) grew by 2.3 GB for the same 1 GiB
container write and was back to where it started 30 s later: that, the
framework does not allow. Details and the experiments behind this: [bench/RESULTS.md](../bench/RESULTS.md#the-macos-vms-memory-a-ceiling-instead-of-a-ratchet-2026-10-19).

### Disk on macOS

The VM's data disk is a sparse file, `vm-state/disk.img` in the dev data
directory (64 GiB at most; only what the guest writes takes space). Unlike
its memory, the framework does give disk space back: the virtio disk
accepts discards (TRIM) and punches the discarded blocks out of the file.
So the guest trims its filesystem (FITRIM on `/var/lib/litek8s`) at boot
and then whenever the disk has been quiet for 30 s, at most once a minute;
ext4 only discards block groups freed since the last trim, so an idle
trim costs nothing. (Mounting with `discard` would discard on every
delete, in the middle of pulls and container churn.) `docker rmi` and
`docker image prune` free the image's layers at once (layers written in
the last 10 minutes wait for the node's image GC, in case a pull still
needs them), and the Mac has the space back within a minute or so:

| `disk.img` on the Mac (`du`) | before | now |
|---|---|---|
| a container writes 1 GiB, then `docker rm` | stays at 2.6 GB | back to 1.3 GB 55 s later |
| `docker rmi python:3.13` (1.1 GB unpacked) | stays at 1.54 GB | 1.29 → 0.78 GB within 3 min; 123 MB after the node's next image GC (9 min later) |
| boot with 362 MB deleted from an older guest | – | given back by the first trim |

`du` of `disk.img` is what counts; `ls` shows the image's 64 GiB size.

### Data directory

The dev home is `dev/` in litek8s's data directory:
`~/Library/Application Support/dev.litek8s` on macOS,
`~/.local/share/litek8s` on Linux (`$LITEK8S_DEV_HOME` overrides the dev
home). The guest assets built from source go in `vm/` next to it
(`$LITEK8S_DEV_VM_ASSETS` overrides).

`LITEK8S_HOME=<dir>` puts all of litek8s's per-user files under `<dir>`
instead: the data directory becomes `<dir>/data` (so the VM, its disk and
the clusters are `<dir>/data/dev`, the guest `<dir>/data/vm`), the Docker
sockets and `config.yaml` are `<dir>/...` instead of `~/.litek8s/...`, the
kubeconfig is `<dir>/kubeconfig` (unless `KUBECONFIG` is set) and contexts
are `lkdev-<cluster>` (`$LITEK8S_CONTEXT_PREFIX`). Two litek8s with
different homes run two VMs that know nothing of each other; that is how a
build from a checkout (`dev/lk`) runs next to an installed release
([develop-without-docker.md](develop-without-docker.md#using-litek8s-vs-developing-it)).

Builds before the rename used `~/Library/Application Support/dev.civojairo.litek8s`
on macOS. litek8s keeps using that directory as long as it exists and
`dev.litek8s` does not, so an existing VM, its disk and its clusters are
never moved behind your back. To move to the new name:

```bash
litek8s dev list            # note the clusters that are up
litek8s dev down --all      # every cluster; the VM powers off with the last
pgrep -fl 'litek8s dev-vm'  # nothing left
mv ~/Library/Application\ Support/dev.civojairo.litek8s ~/Library/Application\ Support/dev.litek8s
litek8s dev up              # same clusters, objects and images
```

## Settings

`dev up` takes its defaults from `~/.litek8s/config.yaml` (`$LITEK8S_CONFIG`),
which `litek8s ui` edits ([ui.md](ui.md)); flags win over it:

| setting | flag | macOS | Linux |
|---|---|---|---|
| `cpus` | `--cpus` | the VM's CPUs (default: half the Mac's, at most 8) | a CPU limit on the sandbox (default: none) |
| `memory` (MiB) | `--memory` | the VM's ceiling (default 4096) | a memory limit on the sandbox (default: none) |
| `shares` | `--share` (repeatable) | directories shared at the same path (default `$HOME`) | - (the sandbox sees the host's files) |
| `port` | `--port` | API server port of the default cluster (default 6443, a free one if taken; other clusters keep the one they got) | same |
| `publish` | `--publish` | `localhost` (default) or `all`: where NodePorts, LoadBalancer ports and `docker run -p` ports listen | same |

The file is read at `up`; `env.json` records what the VM or sandbox started
with, each cluster's `dev.json` its own port and nodes. `cpus`, `memory`,
`shares` and `publish` belong to the VM or sandbox all clusters share: they
apply when the first cluster starts it.

## Images

- `litek8s dev load <image>` exports the image from the local Docker (or
  Podman's Docker API, via `DOCKER_HOST`) and imports it; `litek8s dev load
  file.tar` takes `docker save` / `podman save` archives in Docker or OCI
  layout, including Docker 25+'s multi-platform archives (only this host's
  platform is used). Loaded images are never pulled over, whatever the pull
  policy, so `myapp:latest` works without `imagePullPolicy: IfNotPresent`.
- A registry on your machine's `localhost` works as it is:
  `image: localhost:5000/myapp:1` reaches the host's port 5000 from inside the
  sandbox (pasta maps the gateway to the host's loopback) or the VM (the
  framework's NAT gateway; the registry must listen on all interfaces, which
  `docker run -p 5000:5000 registry:2` does).
  Other plain-HTTP registries: `LITEK8S_INSECURE_REGISTRIES=host:port`.
- Private registries (a CA of your own, credentials, plain HTTP) and
  mirrors: `~/.litek8s/registries.yaml`, k3s's format, which `dev up` gives
  the cluster's nodes at each start ([registries.md](registries.md)).

## Several clusters

kind and k3d give every cluster its own node container (a whole systemd or
k3s with its own containerd, ~400-600 MB each); `litek8s dev` gives it a
few processes in the VM or sandbox that is already running:

```bash
litek8s dev up --name ci                 # context litek8s-ci, its own API port, data and Docker socket
litek8s dev up --name multi --nodes 3    # 3 nodes: litek8s-multi, -worker, -worker2
litek8s dev list
NAME         STATE    API               NODES  MEMORY     AGE   CONTEXT
dev          running  127.0.0.1:6443    1      4.2 MiB    7m    litek8s-dev
ci           running  127.0.0.1:36097   1      3.9 MiB    1m    litek8s-ci
multi        running  127.0.0.1:41011   3      17.7 MiB   0s    litek8s-multi

shared sandbox (rootless, 62.3 MiB in use)
litek8s dev down --name ci               # stop one; the VM/sandbox stops with the last one
litek8s dev delete --name ci             # and delete its data and context
litek8s dev down --all
```

`dev up|down|status|delete|reset|load` take `--name` (default `dev`, which
keeps the paths, port, context and socket litek8s always used, so an
existing cluster is not migrated). What each cluster gets:

| | default (`dev`) | `--name <n>` |
|---|---|---|
| kubectl context | `litek8s-dev` | `litek8s-<n>` (`--no-switch` adds it without making it current) |
| API port | 6443, a free one if taken | the one it had last, else a free one |
| Docker socket | `~/.litek8s/docker.sock` | `~/.litek8s/<n>/docker.sock` (docker context `litek8s-<n>`) |
| data (Linux) | `~/.local/share/litek8s/dev/cluster` | `~/.local/share/litek8s/dev/clusters/<n>/data` |
| data (macOS) | `/var/lib/litek8s/dev` in the VM's disk | `/var/lib/litek8s/<n>` in the VM's disk |
| log | `dev.log` in the dev data directory | `clusters/<n>/dev.log` there (the VM's or sandbox's own: `env.log`) |
| node names | `litek8s-dev` | `litek8s-<n>`, `litek8s-<n>-worker`, `-worker2`, ... |
| NodePorts from | 30000 | 30100-32700, by name (so two clusters rarely want the same `localhost` port) |

**How.** The first `dev up` starts the VM (macOS) or the sandbox (Linux);
each cluster is then a *cell* in it, made by the same code that makes the
Linux sandbox: its own network namespace (its `litek8s0` pod bridge,
iptables rules, DNS, kubelet and NodePort ports: every cluster uses the
usual 10.42.0.0/16 pods and 10.43.0.0/16 Services without conflict), mount
namespace (`/run`, crun state), PID namespace (stopping it stops all of its
processes), cgroup namespace (`/cells/<cluster>/<node>`: a cluster's memory
is that directory's), and UTS and IPC namespaces. Cells reach each other and
the outside through a bridge in the VM or sandbox (`lkcells0`,
10.231.0.0/24, masqueraded). A small manager inside (`litek8s dev __cells`)
starts and stops cells; the VM process or supervisor outside binds each
cluster's API port and Docker socket on the host (Linux: handed to the cell
as descriptors, so connections are not copied; macOS: relayed over vsock into
the cell's network namespace) and forwards its NodePorts. When the last
cluster stops, the VM or sandbox goes too.

**Several nodes.** `--nodes N` (1-10, remembered) runs N-1 workers, each in
a cell of its own, joined with the real join flow (`litek8s agent --join
https://<control plane>:6443 --token ...`, see
[multi-node](../compat/cases/M1-multinode.sh)) and connected by the same
WireGuard pod overlay real nodes use: test pod spreading, affinity,
DaemonSets, draining, or a node failing on your laptop:

```bash
litek8s dev down --name multi --node litek8s-multi-worker2   # its pods die; NotReady after the grace period (40 s)
litek8s dev up --name multi --node litek8s-multi-worker2     # back, Ready once it reconnects
```

 On Linux the overlay needs
the `wireguard` kernel module loaded (`lsmod | grep wireguard`; a rootless
sandbox cannot load it; most distributions load it on first use by root,
e.g. `sudo modprobe wireguard` once).

**Isolation, honestly.** Cells isolate names, networks, files under `/run`,
processes and cgroups, as containers do; they do not isolate the kernel. All
clusters share:

- the VM's CPUs and memory (macOS) or the sandbox's limits (Linux,
  `cpus`/`memory` in the settings): set for all of them, and one busy
  cluster slows the others (no per-cluster limits yet);
- the VM's disk: one sparse image for every cluster; what they delete goes
  back to the Mac within a minute or so (see [Disk on macOS](#disk-on-macos));
- on Linux, one user namespace: the same subordinate ids map to the same
  host ids in every cluster;
- the host's ports: NodePorts, LoadBalancer ports and `docker run -p` ports
  of every cluster listen on the same `localhost`; when two clusters want
  one port, the first gets it and the other's log says "not exposed";
- the image store is per cluster: an image pulled or `dev load`ed in one
  is not in the others (`dev load --name <n>`).

A cluster deleted while the VM is down has its data deleted at the VM's next
boot.

### Memory: 1, 3 and 5 clusters

Each cluster one node, idle 60 s after `dev up` (or `kind create cluster`,
`k3d cluster create`), then with a 3-replica `nginx:alpine` deployment in each
(60 s after the rollout). A cluster's memory is counted the way `docker
stats` counts a container: its cgroup's usage less the page cache the
kernel would drop first. Measured 2026-10-19 with the release build.

**macOS** (M-series MacBook, 14 cores, 36 GB; litek8s VM at `--cpus 4
--memory 4096`; kind v0.33.0 and k3d v5.8.3 with their default images on
OrbStack, as installed):

| clusters | litek8s: whole VM¹ | litek8s: the clusters² | kind: node containers³ | k3d: server + LB containers³ |
|---|---|---|---|---|
| 1, idle | 375 MiB | 2.4 MiB | 564 MiB | 614 MiB |
| 3, idle | 378 MiB | 6.1 MiB | 1574 MiB | 1707 MiB |
| 5, idle | 386 MiB | 9.2 MiB | 2857 MiB | 2818 MiB |
| 1, 3 pods each | 452 MiB | 28 MiB | 671 MiB | 649 MiB |
| 3, 3 pods each | 697 MiB | 82 MiB | 2033 MiB | 1925 MiB |
| 5, 3 pods each | 898 MiB | 135 MiB | 3080 MiB | 3208 MiB |

¹ The VM's physical footprint as macOS counts it (what Activity Monitor
shows for it): the guest kernel, the initramfs (the litek8s binary, crun,
iptables) held in RAM, page cache, and every cluster. ² The sum of the
clusters' cells. ³ `docker stats`, without OrbStack's own VM, which the
kind and k3d columns do not include (it runs anyway for Docker). Creating
the clusters took 9/32/54 s with kind, 17/50/83 s with k3d, and 0.1-0.2 s
each with litek8s once the VM was up (its boot: about 1.5-2.5 s, once).

**Linux** (omarchy: x86_64, 16 threads, 32 GB, kernel 7.2, rootless; kind
and k3d are not installed there, so litek8s only):

| clusters | litek8s: whole sandbox⁴ | litek8s: the clusters² |
|---|---|---|
| 1, idle | 38 MiB | 4 MiB |
| 3, idle | 52 MiB | 13 MiB |
| 5, idle | 63 MiB | 20 MiB |
| 1, 3 pods each | 91 MiB | 57 MiB |
| 3, 3 pods each | 205 MiB | 166 MiB |
| 5, 3 pods each | 317 MiB | 273 MiB |

⁴ The sandbox's cgroup: the supervisor, pasta, the cell manager and every
cluster (and, on Linux, the page cache of the binary, charged to whoever
touched it first).

What it means: an idle cluster costs 2-4 MiB beyond the first and a kind
or k3d cluster about 550 MiB, so five idle litek8s clusters fit in the
memory of one kind node with room to spare. With pods the pods dominate:
nginx starts a worker per CPU the cell sees (4 in the VM, 16 on the Linux
machine). The VM's footprint grows faster than its clusters' sum with
workloads: each cluster pulls and unpacks its own images (the image store is
per cluster), and the guest's page cache stays in the footprint until macOS
asks for it back.

## Limits

- No authentication on the API (it listens on 127.0.0.1 only; inside the
  VM or sandbox, on the cluster's cell, and for multi-node clusters also on
  its cell address, which only the other cells reach).
- hostPorts are not exposed on the host's localhost (NodePorts are); rootless
  cannot bind host ports below 1024.
- macOS: the first boot formats the data disk (~5 s); DHCP from the
  framework's NAT occasionally takes 2–3 s when no other VM has run for a
  while.
