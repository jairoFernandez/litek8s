# litek8s vs k3s on a small node — baseline

Baseline before replacing the Docker runtime (crun/litecontainer) and moving to a
push-based resident agent. Raw data:
[`results/2026-10-16-t4g-micro-shape/`](results/2026-10-16-t4g-micro-shape/)
(`report.md` there is the full generated report).

## Headline

One fresh guest per system: Ubuntu 24.04 arm64, **2 vCPU, 1 GiB RAM, no swap**
(the shape of an AWS t4g.micro). All three run the same `nginx:alpine` workload
with the same limits (500m CPU / 128Mi) and the same TCP readiness probe.

| metric | litek8s + Docker | k3s minimal¹ | k3s default |
|---|---|---|---|
| Idle orchestrator memory, 0 workloads (PSS) | **128 MiB** | 572 MiB | 693 MiB |
| Idle orchestrator memory, 5 nginx (PSS) | **188 MiB** | 603 MiB | 698 MiB |
| Guest RAM left (MemAvailable), 5 nginx | **627 MiB** | 321 MiB | 59 MiB |
| Idle CPU, 0 workloads (whole guest, % of one vCPU) | **0.9 %** | 4.2 % | 6.3 % |
| Idle CPU, 5 nginx (whole guest, % of one vCPU) | 5.8 % | **5.2 %** | 20.4 % |
| Apply → first ready container (median of 10, warm image) | **0.16 s** | 0.79 s | 1.75 s |
| Disk used after install (no workload images) | **285 MiB** | 406 MiB | 1,064 MiB |
| Orchestrator binary | 22.6 MiB | 68.1 MiB | 68.1 MiB |

¹ `--disable=traefik,servicelb,metrics-server,local-storage`: only CoreDNS left as an addon.
This is the real competitor; the default install barely fits 1 GiB (59 MiB left with 5 pods,
and part of its 20 % CPU is the kernel reclaiming memory).

Memory breakdown (PSS, MiB, 0 → 5 workloads):

| | control plane | runtime | addons |
|---|---|---|---|
| litek8s | litek8s serve 11 → 14 | dockerd 73 → 89, containerd 44 → 58, 5 shims 28 | – |
| k3s minimal | k3s-server 400 → 402 | containerd + shims + pause 117 → 144 | CoreDNS 55 |

**Read:** litek8s wins memory (3–4.5× less than k3s minimal), disk, startup latency and the
empty-cluster CPU. It already **loses on CPU at 5 workloads**, and that gap grows with every
deployment, because of its own polling (overhead #2 below), not because of Docker.

## After cutting steady-state polling (`cfb4222`)

Rerun of the litek8s and remote-node phases (`bench/run.sh litek8s rpc`) on fresh guests of
the same shape, after fixing overheads #2, #4 and #6 below. Raw data:
[`results/2026-10-16-after-polling-cut/`](results/2026-10-16-after-polling-cut/).
"Before" is the baseline above (`78fc083`); the run also includes the reconciler-storm fix
(`78b42d3`), which does not touch the steady-state path.

| metric | before | after | k3s minimal |
|---|---|---|---|
| Idle CPU, 5 nginx (whole guest, % of one vCPU) | 5.8 % | **2.55 %** | 5.2 % |
| … of which dockerd + containerd + shims | 5.27 % | 2.14 % | – |
| … of which litek8s serve | 0.59 % | 0.41 % | – |
| containerd `write()` traffic, 5 nginx | 12.6 MiB/min | 6.1 MiB/min | – |
| Idle CPU, 0 workloads | 0.9 % | 0.81 % | 4.2 % |
| Remote node: RPCs/s, 5 deployments at steady state | 14.9 | **2.1** | – |
| Remote node: RPCs per reconcile pass | 11.2 | 1.9 | – |
| Remote node: node VM busy | 13.3 % | **3.41 %** | – |
| Remote node: control-plane VM busy | 7.8 % | **1.89 %** | – |
| Apply → first ready container (median) | 0.16 s (10 runs) | 0.21 s (5 runs) | 0.79 s |
| Idle orchestrator memory, 5 nginx (PSS) | 188 MiB | 183 MiB | 603 MiB |

litek8s now uses **half the idle CPU of k3s minimal** with 5 workloads, instead of losing to it.
Apply → ready shows the same bimodal ~0.17 s / ~0.34 s split as before (the slow runs are
slow in the CLI apply itself); with 5 vs 10 runs the median shift is within noise.

RPC census after the change (5 deployments on the SSH node, 60 s):

| request | per second (before → after) | why |
|---|---|---|
| `StatsSnapshot` | 7.50 → **0** | stats only while a `top` client is watching |
| `ListContainersForDeployment` | 2.58 → **0** | replaced by the per-node list |
| `ListManagedContainers` | 0.50 → 0.48 | one per node per pass, shared by cleanup and all replicas |
| `ProbeContainerTcp` | 2.52 → 0.43 | Ready replicas re-probed every 10 s, new containers at once |
| `Ping` | 1.62 → 1.00 | no snapshot refresh after each reconcile pass, 1 s ticker only |
| `HostResources` | 0.18 → 0.17 | unchanged (5 s cache) |

What changed:

- **Snapshot loop**: per-container `docker stats` is collected only while a `top` request
  arrived in the last 15 s. The first `top` after a quiet period refreshes the snapshot
  inline, so it still shows memory and network numbers (CPU % needs a second sample, as
  before). The extra snapshot refresh after every reconcile pass is gone.
- **Reconciler**: one `ListManagedContainers` per node per pass, grouped by deployment and
  ordinal and shared with orphan cleanup, instead of one list per replica plus one for cleanup.
- **Readiness probe**: a Ready replica whose container ID has not changed is probed every
  10 s instead of every 2 s pass. A new container is probed on the next pass. A failing probe
  is still retried every pass.
- **Raft**: `SetReplicaAssignment` / `SetReplicaStatus` are proposed only when the store says
  they would change something. The check uses the same transition logic, including the
  time-based restart-streak reset. An unchanged pass now adds no Raft log entries.

What is left at idle: on the local node, dockerd + containerd still do 2.1 % for one container list
per pass, one Docker ping per second (node health in the snapshot loop) and a probe per
deployment every 10 s. On the remote node the remaining cost is almost all ssh + `litek8s
agent` exec per RPC (overhead #3), not the RPC count.

## The Kubernetes path: `litek8s apiserver` + node agent (2026-10-17)

The same harness (`bench/run.sh kube`, new phase) on the same VM shape, for
the path litek8s is moving to: `litek8s apiserver` with its controllers,
scheduler, cluster DNS and the local node agent in one process (systemd unit
`litek8s`), on Docker. The workload is the same 5 `nginx:alpine` Deployments
as for k3s (requests 50m/32Mi, limits 500m/128Mi, TCP readiness probe every
2 s), created through the Kubernetes API. Raw data:
[`results/2026-10-17-kube/`](results/2026-10-17-kube/) (before) and
[`results/2026-10-17-kube-after/`](results/2026-10-17-kube-after/) (after the
fixes below); k3s columns from
[`results/2026-10-16-t4g-micro-shape/`](results/2026-10-16-t4g-micro-shape/).

| metric | kube path, before | kube path, after | litek8s daemon (`cfb4222`) | k3s minimal |
|---|---|---|---|---|
| Idle orchestrator memory, 0 workloads (PSS) | 176.6 MiB | **166.1 MiB** | 128 MiB | 572 MiB |
| Idle orchestrator memory, 5 nginx (PSS) | 214.6 MiB | **208.8 MiB** | 183 MiB | 603 MiB |
| … of which the litek8s process, 0 → 5 nginx | 15.0 → 17.7 MiB | **14.5 → 17.5 MiB** | 11 → 14 MiB | k3s-server 400 → 402 MiB |
| Guest RAM left (MemAvailable), 5 nginx | 587 MiB | **602 MiB** | 627 MiB | 321 MiB |
| Idle CPU, 0 workloads (whole guest, % of one vCPU) | 0.17 % | **0.18 %** | 0.81 % | 4.2 % |
| Idle CPU, 5 nginx (whole guest, % of one vCPU) | 0.49 % | **0.46 %** | 2.55 % | 5.2 % |
| Apply → first Ready pod (median of 5, warm image) | 2.19 s | **0.20 s** | 0.21 s | 0.79 s |
| Disk used after install (no workload images) | 324 MiB | 324 MiB | 285 MiB | 406 MiB |
| Orchestrator binary | 31.1 MiB | 31.1 MiB | 22.6 MiB | 68.1 MiB |

**Read:** the Kubernetes path keeps the daemon's footprint (a third of k3s
minimal's memory) and uses **a tenth of k3s's idle CPU**: the API server,
controllers and agent are event-driven end to end (store change feed,
Docker's event stream, one pushed connection to the agent), so with 5 pods
the litek8s process itself averages 0.08 % of a vCPU, almost all of it the
readiness probes. It is now as fast as the daemon from apply to Ready and 4×
faster than k3s.

What changed between the two runs:

1. **Readiness probes start fast** (`agent/probe.rs`). The first check ran
   the instant the container started, failed (nginx not listening yet), and
   the next waited the full `periodSeconds` (2 s). Until the first success
   readiness is now re-checked after 25 ms, doubling up to the period; a
   readiness failure before the first success changes nothing, so the
   thresholds keep their meaning (startup and liveness keep the plain
   period). Apply → Ready **2.19 s → 0.20 s**.
2. **No helper container unless a Service needs routing, and none on a
   Linux host** (`agent/runtime.rs`, `agent/proxy.rs`). The ClusterIP NAT
   rules go straight into the host's table with `iptables-restore` when the
   agent is root next to a local Docker; the privileged helper container is
   only for VM-backed Docker. Idle memory **−10 MiB** (its containerd-shim).
3. **Watch history as compact JSON** (`kube/store.rs`). Every retained
   version used to be a parsed object pinned twice (history and the change
   feed's ring). History now keeps serialized bytes (parsed back only when a
   watch resumes) and the live feed is 256 slots. On a 60-pod create/delete
   churn the live heap dropped from 14.8 MiB to 5.9 MiB after everything was
   deleted (19.2 → 12.2 MiB with the 60 pods running); with 5 quiet pods it
   does not show.

Also found by running it on a stock Ubuntu guest, and fixed before the
"before" numbers: the TLS material was written before the data directory
existed (TLS silently off, so no `kube-root-ca.crt` and every pod stuck),
cluster DNS could not bind `0.0.0.0:53` next to systemd-resolved, and pods
could not reach an API server listening on loopback only. DNS and the
in-cluster API port now also bind the Docker gateway address pods use.

Biggest remaining overhead: Docker runs one containerd-shim per container,
and a pod is two containers (the sandbox that owns the network namespace,
plus nginx), so 5 pods cost ~40 MiB of shims where the daemon's 5 bare
containers cost 28 MiB and k3s's CRI groups a pod under one shim. A runtime
that shares one shim per pod (containerd's CRI shim, or crun/litecontainer
behind the `PodRuntime` trait) removes it.

## The Kubernetes path on crun, without Docker (2026-10-17)

Same harness and VM shape; new phase `bench/run.sh kube-crun` runs the same
`litek8s apiserver` with `--runtime crun`: pods run with crun directly
(images, overlayfs root filesystems, pod network namespaces, logs and exit
codes handled by litek8s itself; one small `litek8s-shim` per container).
Both litek8s columns come from one run on fresh guests
([`results/2026-10-17-kube-crun/`](results/2026-10-17-kube-crun/), `b5bb4b2`);
k3s minimal from [`results/2026-10-16-t4g-micro-shape/`](results/2026-10-16-t4g-micro-shape/).

| metric | kube path on Docker | **kube path on crun** | k3s minimal |
|---|---|---|---|
| Idle orchestrator memory, 0 workloads (PSS) | 169.7 MiB | **14.0 MiB** | 572 MiB |
| Idle orchestrator memory, 5 nginx (PSS) | 209.5 MiB | **24.6 MiB** | 603 MiB |
| … the litek8s process, 0 → 5 nginx | 15.5 → 18.3 MiB | 14.0 → 17.2 MiB | k3s-server 400 → 402 MiB |
| … the container runtime, 0 → 5 nginx | 154.2 → 191.2 MiB | **0 → 7.4 MiB** (5 shims) | 117 → 144 MiB |
| Guest RAM left (MemAvailable), 5 nginx | 604 MiB | **663 MiB** | 321 MiB |
| Idle CPU, 0 workloads (whole guest, % of one vCPU) | 0.20 % | **0.08 %** | 4.2 % |
| Idle CPU, 5 nginx (whole guest, % of one vCPU) | 0.48 % | **0.21 %** | 5.2 % |
| Apply → first Ready pod (median of 5, warm image) | 0.22 s | **0.08 s** | 0.79 s |
| Disk used after install (no workload images) | 325 MiB | **41.5 MiB** | 406 MiB |
| Orchestrator binary | 32.5 MiB | 32.5 MiB | 68.1 MiB |

**Read:** with 5 pods the whole orchestrator (control plane, scheduler,
controllers, DNS, service proxy, node agent and container runtime) takes
**24.6 MiB, 8.5× less than on Docker and 24× less than k3s minimal**, and
the guest has 59 MiB more free memory than on Docker. The runtime side shrank
from 191 MiB (dockerd 89, containerd 59, ten containerd-shims 41, five sandbox
`sleep` containers 3) to five shims of ~1.5 MiB PSS each (3.7 MiB RSS, most of
it the shared litek8s binary): there is no pause/sandbox process at all, the
pod's network and IPC namespaces live as bind mounts. Nothing in the runtime
polls (exits arrive through a pidfd per shim), so the idle runtime CPU is 0
and the guest idles at 0.21 % with 5 pods, a 25th of k3s. Apply → Ready drops
to 80 ms because a pod start is now `unshare` + one `ip -batch` + `crun
create/start`, instead of two containers through dockerd → containerd → shim →
runc. Disk after install is the binary plus the crun package (2 MiB): no
Docker. Pulled images are stored once, as unpacked layers only (compressed
blobs are verified while downloading and then dropped).

Compatibility is unchanged: the compat corpus against this runtime scores
L2 15/15, L3 6/7, L4 20/20, the same as on Docker
([`compat/results/20261017-142548-litek8s-crun/`](../compat/results/20261017-142548-litek8s-crun/summary.md)), and the same 41/42 on x86_64 (crun in a
privileged Ubuntu 24.04 container on a Linux workstation, amd64 images:
[`results/20261017-150615-litek8s-crun-amd64/`](../results/20261017-150615-litek8s-crun-amd64/summary.md)).

Caveat: the run's `kubecrun.idle5.statefile-bytes.txt` (381 MB) is not the
object log: `du` also counted the image store and descended into the mounted
container root filesystems under `node-agent/crun`. `run.sh` now excludes
them; the object log itself is the same size as on Docker (~270 KB).

**Rerun on the merged code** (main with the 101-case corpus fixes, plus zstd
layers, the pod PID namespaces and the TLS change; `18d0909`,
[`results/2026-10-17-kube-crun-merged/`](results/2026-10-17-kube-crun-merged/)):
the numbers held. 5 nginx: orchestrator 24.7 MiB PSS (litek8s 17.4, five
shims 7.2), 665 MiB MemAvailable, idle CPU 0.23 % (0.08 % with no pods),
apply → Ready 0.06 s median, disk after install 42.2 MiB (the binary grew
0.6 MiB), object log 296 KB with the fixed measurement.

## Local dev cluster: `litek8s dev` vs kind and k3d (2026-10-18)

`litek8s dev up` ([docs/dev.md](../docs/dev.md)) against the two usual
local-cluster tools, same protocol for all three
([`dev-bench.sh`](dev-bench.sh), [`dev-bench-mac.sh`](dev-bench-mac.sh);
raw output in [`results/2026-10-18-dev/`](results/2026-10-18-dev/)):
three fresh clusters each (`dev reset` / `kind delete` / `k3d cluster delete`
in between), node image and `nginx:alpine` already on the machine; then 60 s
idle, then 5 nginx. Memory is `memory.current - inactive_file` of the
cgroups everything the cluster runs in (litek8s: its systemd scope; kind/k3d:
the node containers; the number `docker stats` shows).

**Linux** (Ubuntu 24.04 arm64, qemu+HVF guests of 4 vCPU / 4 GiB on an M3
Max, one fresh guest per tool; kind v0.30.0 with kindest/node v1.34.0, k3d
v5.8.3 with k3s v1.33.4 minus traefik/metrics-server/servicelb, Docker 29.1;
litek8s rootless, crun 1.14, pasta):

| metric | **litek8s dev** | kind | k3d |
|---|---|---|---|
| `up` → node Ready (median of 3) | **0.17 s** | 31.6 s | 11.1 s |
| `up` → first Ready pod (image loaded from a `docker save` archive) | **0.82 s** | 33.7 s | 20.9 s |
| Idle memory, no workloads | **46.8 MiB** | 526 MiB | 398 MiB |
| … plus the Docker engine it needs (dockerd + containerd, anon) | — | +73 MiB | +73 MiB |
| Memory with 5 nginx | **71.1 MiB** | 580 MiB | 428 MiB |
| Idle CPU over 60 s (% of one vCPU) | **0.01 %** | 10.8 % | 5.1 % |
| Disk (binary + data incl. nginx / node image + writable layers) | **105 MiB** | 1782 MiB | 335 MiB |
| Root needed | no (Ubuntu ≥ 23.10: `sudo litek8s dev setup` once, AppArmor) | docker group | docker group |

**macOS** (M3 Max, macOS 27; kind and k3d on OrbStack as installed, used
as is; litek8s in its own Virtualization.framework VM, 2 GiB / 7 vCPU):

| metric | **litek8s dev** | kind on OrbStack | k3d on OrbStack |
|---|---|---|---|
| `up` → node Ready (fresh cluster, median of 3) | **1.28 s** | 34.3 s | 10.4 s |
| `dev down` + `dev up` → pods Ready again | **1.1 s** | – | – |
| `up` → first Ready pod | **2.15 s** | 40.2 s | 17.3 s |
| Idle memory | **209 MiB** VM footprint on macOS (165 MiB used in the guest) | 586 MiB in OrbStack's VM | 431 MiB in OrbStack's VM |
| Memory with 5 nginx | **247 MiB** footprint (196 MiB in the guest) | 639 MiB | 468 MiB |
| Idle CPU over 60 s (% of one core) | **0.18 %** (the whole VM) | 12.0 % | 3.9 % |
| Disk | 318 MiB (kernel 34 + initramfs 23 + binary 29 + signed copy 30 + data disk) | 973 MiB | 219 MiB |

kind and k3d numbers exclude OrbStack's own VM, which is running anyway
(here 3.0 GiB RSS shared with other work on this machine); the litek8s
column is everything, VM included. Measured on a shared machine (load 10–25
from other sessions), so timings are pessimistic for all three.

**Read:** on Linux the whole cluster costs about what one kind control-plane
component does: 47 MiB and no measurable idle CPU, against 400–530 MiB and
5–11 % for k3d and kind (and their Docker engine), and it is ready before
`kind create cluster` has pulled its first layer: 0.17 s to a Ready node,
0.8 s to a Ready pod. There is nothing to boot: `dev up` forks a sandbox
(user, PID, network, mount and cgroup namespaces; pasta for egress) and the
API server, controllers and crun node start in-process. On macOS the price of
Linux is a VM, and it is small: Alpine's `linux-virt` kernel and a 23 MiB
initramfs reach the API server in ~0.5 s (the API and NodePorts go over
vsock, so readiness does not wait for DHCP), 0.9–1.3 s end to end, and the
VM's whole footprint is 209 MiB, below the in-VM usage of either Docker-based
cluster alone.

Caveats: the macOS memory is the VM process's physical footprint, which
keeps pages the guest touched at boot (unpacking the initramfs) even after
the guest frees them; a balloon "squeeze" to hand them back made it worse
(the guest zeroes balloon pages; see the next section for what the balloon
can and cannot do). k3d on OrbStack ran with kubelet eviction
thresholds lowered to 1 % because OrbStack's disk was 91 % full from other
work (the default 10 % put the node in DiskPressure); kind ran before that.
kind could not `load image-archive` Docker 29's default (multi-platform)
`docker save` output, so every tool loaded `docker save --platform
linux/arm64`; `litek8s dev load` takes both.

Compatibility is the same as the plain API server's: the 101-case corpus
passes 101/101 against `litek8s dev` both on Linux (rootless) and in the macOS
VM ([compat/RESULTS.md](../compat/RESULTS.md); k3d is the 101/101 reference
there). kind was not scored; it runs upstream Kubernetes.

## The macOS VM's memory: a ceiling instead of a ratchet (2026-10-19)

The Home view showed "cache 2.0 GB reclaimable" for the dev VM, and it was
reclaimable inside Linux but not on the Mac: with the VM at `--memory 4096`,
Argo CD and a demo load, `echo 3 > /proc/sys/vm/drop_caches` in the guest
freed 1.5 GB and the VM process's footprint stayed at 2885 MB.

**What Virtualization.framework does (macOS 27, M-series, 36 GB).** Every
guest page touched once stays charged to the VM's XPC helper until the VM
stops. Measured here:

| experiment | footprint |
|---|---|
| fresh VM, then 2.4 GB written to a file in a pod, deleted, caches dropped | 366 → 2927 → 2927 MB |
| then the balloon inflated by 3 GiB (target 1 GiB), Alpine's default `init_on_alloc=1` | 2927 → 3464 MB: the driver zeroes the pages it takes, and nothing comes back |
| fresh VM with `init_on_alloc=0`, balloon inflated by 3 GiB | 281 → 289 MB: free, as the pages were never touched |
| then 2.5 GB written through the page cache under that 1 GiB ceiling | 289 → 1046 MB (without the ceiling: 2927) |
| save + stop + restore + resume of a 2.3 GB-footprint VM using ~300 MB | 6.8 s pause (save 3.3 s, 2.1 GB state file); the new helper's footprint: 4251 MB (restore brings all of it in) |

The framework's balloon offers MUST_TELL_HOST and DEFLATE_ON_OOM only
(`/sys/bus/virtio/devices/*/features`): no free page reporting, no free
page hints, no statistics; Lima and ArcBox report the same, and OrbStack and
Docker Desktop got dynamic memory by moving to their own VMMs on
Hypervisor.framework. So litek8s cannot hand memory back; what it can do is
keep the guest from touching memory it does not need:

- the VM boots with a 1 GiB ceiling (a quarter of `--memory` when that is
  more); the balloon takes the rest before the guest touches it
  (`init_on_alloc=0` on the guest's command line);
- the guest reports MemAvailable, the balloon and PSI on the console
  (`LITEK8S_MEM`: every 2 s on change, 250 ms when low, at once on a PSI
  trigger of 100 ms stalled in 1 s); `litek8s dev-vm` grows the ceiling by
  half when less than 256 MiB is available or memory stalls while less than
  that is free (a first version also grew on stalls with gigabytes free:
  during the compat corpus, pods at their own memory limits stalled and the
  ceiling went to 4 GiB for nothing), and to the
  whole VM when it is nearly out; it shrinks it by half the excess over
  512 MiB available, at most once a minute, after 2 calm minutes
  (`src/dev/balloon.rs`);
- `lru_gen/min_ttl_ms=1000` in the guest: without it, a 1 GiB burst under
  the 1 GiB ceiling evicted the code of every process (the root filesystem
  is a disk now) and the guest thrashed for 60 s, long enough for the node
  agent to lose its connection; with it the kernel takes its OOM path, the
  balloon deflates there (DEFLATE_ON_OOM), and the burst took 0.40 s;
- a smaller fixed cost: the root filesystem is a 53 MiB romfs image on a
  read-only virtio disk with a tmpfs overlay (a 753 KiB initrd mounts it)
  instead of a 56 MiB initramfs in RAM plus the 25 MiB archive the
  framework loads for it, and `swiotlb=noforce` drops 64 MiB of bounce
  buffers the kernel zeroed at boot (virtio devices here do not need them).

**Results** (`bench/vm-memory.sh`, `--cpus 4 --memory 4096`, a fresh
cluster each, a `python:3.13-alpine` pod; before: bf1f538's guest and host,
the same behaviour as main before this change):

| phase | before: footprint | before: guest used / cache | now: footprint | now: guest used / cache | now: ceiling |
|---|---|---|---|---|---|
| idle, 2 min after `dev up` | 461 MB | 200 / 121 MiB | **389 MB** | 227 / 128 MiB | 1024 MiB |
| Argo CD v3.1.9 (manifests), 2 min after rollout | 1847 MB | 1586 / 1248 MiB | **1140 MB** | 834 / 513 MiB | 1024 MiB |
| `docker pull python:3.13` (1.0 GB) | 3296 MB | 2837 / 2407 MiB | **1200 MB** | 790 / 436 MiB | 1024 MiB |
| 5 min idle | 3296 MB | 2837 / 2408 MiB | **1210 MB** | 786 / 436 MiB | 1024 MiB |
| 1 GiB allocated and touched in the pod | 0.39 s, 4083 MB | | **0.36 s**, 1596 MB | | 4096 MiB |

No OOM kill in either run; the node stayed Ready. The compat corpus passes
119/119 on this VM
([compat/results/20261019-224949-litek8s-dev-macos-vm-balloon](../compat/results/20261019-224949-litek8s-dev-macos-vm-balloon/summary.md)). Guest used is MemTotal -
MemFree - balloon (cache included). Argo CD's seven pods and litek8s fit in
the 1 GiB ceiling with half of it page cache, so the ceiling never moved
until the burst.

- Idle with nothing deployed: 384 → 295 MB.
- 1 GiB written, read and deleted in a container (`docker run alpine`): 384
  → 1442 MB before, 295 → 1028 MB now; on OrbStack's VM (running other
  sessions' clusters, not reconfigured) 4052 → 6325 MB and back to 4065 MB
  30 s later.
- Shrinking: after a 1.5 GiB burst grew the ceiling to 4096 MiB, it came
  down to 2471, 1662, 1257 and 1052 MiB, a minute apart, starting 2 minutes
  after the burst; a 600 MB file read every 10 s in a pod stayed cached the
  whole time (124–130 ms per read, `workingset_refault_file` unchanged).

What is left: the footprint only grows. After a peak (a big build) the VM
stays charged for it until `litek8s dev down && litek8s dev up` (~3 s plus
pods restarting); the TUI says so ("freed, still charged ... macOS keeps it
until the VM restarts"). The kernel's page structures (64 MiB for 4 GiB)
and image (~35 MiB) are the rest of the fixed cost; a 16K-page guest
kernel would cut the first by four, but Alpine ships none and some arm64
binaries (jemalloc's, for one) assume 4K pages; not tried.

## Argo CD on k3s and on litek8s (2026-10-19, x86_64/KVM)

Does the advantage hold for a heavy, real workload? Argo CD's application
controller watches every resource type in the cluster, lists them all on
start, patches its Applications on every reconcile and records events for
every sync: a good stress of list/watch, discovery and custom resources.

**Setup.** `bench/run.sh argocd-k3s argocd-litek8s`. Each system gets a
fresh Ubuntu 24.04 **amd64** guest of **2 vCPU / 2 GiB**, no swap, under
qemu + KVM on an AMD Ryzen 7 5800X workstation (16 threads, 31 GiB; the VMs
run inside a Docker container given `/dev/kvm`). k3s v1.36.5+k3s1 minimal
(`--disable=traefik,servicelb,metrics-server,local-storage`: CoreDNS is its
only addon) against `litek8s apiserver --runtime crun` (crun 1.14.1) at
`76dc2cb`. Both install the same pinned chart, **argo-cd 10.10.5** (Argo CD
v3.5.5, chart defaults, non-HA: application controller, ApplicationSet
controller, server, repo server, notifications controller, Dex, Redis; no
PVC), with the same helm v3.19.0, kubectl v1.34.1 and driver
([`node/argocd.py`](node/argocd.py)). The GitOps steps use
`argoproj/argocd-example-apps` (`guestbook`, plain manifests; `helm-guestbook`,
a Helm chart) with automated sync, prune and self-heal. Three rounds,
alternating which system goes first (k3s → litek8s, litek8s → k3s,
k3s → litek8s), three repeats of every timed step per round, 60 s settle and
a 300 s idle window. The host was otherwise idle: 1.9–3.3 % of its CPU busy
during each phase, VM included, load average ≤ 0.8. Raw data:
[`results/2026-10-19-argocd-x86-kvm/`](results/2026-10-19-argocd-x86-kvm/)
(`report.md` is the generated report with every round).

Medians over the three rounds (times: over every repeat; range in parentheses):

| | k3s minimal | **litek8s (crun)** |
|---|---|---|
| Orchestrator memory, no Argo CD (PSS) | 586 MiB | **19 MiB** |
| Orchestrator memory, Argo CD + 1 synced app, idle | 900 MiB | **90 MiB** |
| … control plane (k3s-server / litek8s apiserver) | 736 MiB | 76 MiB |
| … container runtime (containerd, shims, pause / crun shims) | 136 MiB | 13 MiB |
| … CoreDNS | 26 MiB | – (in the API server) |
| Orchestrator memory, peak while syncing (5 s samples) | 945 MiB | **130 MiB** |
| Argo CD's own pods, idle | 342 MiB | 359 MiB |
| Guest MemAvailable, Argo CD + 1 app idle (of 1,967) | 571 MiB | **1,337 MiB** |
| Guest MemAvailable, lowest while syncing | 499 MiB | 1,270 MiB |
| Idle CPU, whole guest, Argo CD + 1 app (% of one vCPU) | 2.98 % | **0.43 %** |
| … control plane / runtime | 1.92 % / 1.03 % | 0.06 % / 0.00 % |
| … Argo CD's pods | 0.36 % | 0.45 % |
| `helm install` → all 7 Argo CD pods Ready, images pulled | 27.1 s (27.0–72.5) | **19.9 s** (19.5–20.0) |
| … after `helm uninstall`, images cached (n=9) | 20.0 s (19.1–44.7) | **13.4 s** (13.3–13.6) |
| … of which `helm install` returning (pre-install hook Job) | 5.8 s | 2.1 s |
| Application created → Synced + Healthy, first (git clone, image pull) | **15.7 s** (15.6–16.6) | 18.1 s (18.0–19.1) |
| … `guestbook` recreated (n=9) | 1.3 s | **1.0 s** |
| … `helm-guestbook` recreated (n=9) | 1.8 s | **1.0 s** |
| Parameter override replicaCount 1 → 2: re-synced, 2 pods Ready (n=9) | 1.9 s (1.6–2.5) | **1.0 s** (1.0–1.0) |
| replicaCount 2 → 1 (n=9) | 0.8 s | 0.7 s |
| Drift: managed Deployment deleted → recreated (n=9) | 0.4 s (0.4–7.2) | 0.3 s (0.3–0.4) |
| … → Available, app Synced + Healthy again (n=9) | 1.1 s (0.8–7.9) | **0.4 s** (0.4–0.4) |
| API requests/s, no Argo CD | 0.77 | 0.02 |
| API requests/s, Argo CD + 1 app idle | 1.22 | 0.10 |
| API requests/s during the GitOps steps (without the driver's own GETs) | 11.5 | 6.4 |
| Open watches, no Argo CD → with Argo CD | 162 → 265 | 0 → 79 |

**Read:** the advantage holds, and grows in absolute terms. With Argo CD
installed and an app synced, the whole litek8s orchestrator takes **90 MiB
against 900 MiB** (10×), leaves 1.34 GiB of the 2 GiB guest free against
0.57 GiB, and idles at a seventh of k3s's CPU. Argo CD itself costs about
the same on both (342 vs 359 MiB, 0.36 vs 0.45 % CPU): the difference is all
orchestrator. Every GitOps step is as fast or faster, and less variable, on
litek8s. k3s minimal does fit at 2 GiB (no OOM kills, no pod restarts on
either system, ~500 MiB still available at its lowest), so no 4 GiB run was
needed. On a 1 GiB node it would not fit: k3s alone already uses ~0.67 GiB
of the 1.92 GiB guest.

**Where the differences come from.**

- *Memory.* k3s-server grows from ~400 to ~740 MiB PSS with Argo CD: its
  watch cache keeps a decoded copy of every watched type, and Argo CD
  watches 103 more of them. The litek8s API server grows from 19 to 76 MiB:
  objects are kept as compact JSON, watches are served from one change feed
  plus a byte-bounded history, and there is no separate kubelet,
  controller-manager, scheduler, CoreDNS or kube-proxy (all in-process).
  Argo CD's application + ApplicationSet controllers take ~45 MiB less on
  litek8s (117 vs 164 MiB), consistent with the application controller's
  cluster cache mirroring every resource type the API serves: litek8s
  serves fewer (Argo CD opens 79 watches against 103). Dex is
  the exception, 93 MiB on litek8s against 28–63 MiB on k3s; not
  investigated.
- *API load.* Argo CD itself is quiet once synced (≈0.1 requests/s on
  litek8s, watch renewals and an Application patch now and then). On k3s
  most of the 1.2 requests/s at idle is k3s talking to itself: lease
  renewals, kubelet node status, endpoint/endpointslice reads, all over
  HTTP even inside one process; plus 60 `/openapi/v3` and 40 discovery GETs
  per 5 minutes that `/metrics` does not attribute (on litek8s Argo CD made
  one discovery request in the same window). While
  syncing, k3s serves twice the requests (11.5 vs 6.4/s): its kubelet and
  controller-manager write Deployment, ReplicaSet and Pod status and events
  through the API, while litek8s's controllers and node agent write straight
  to the store.
- *Install time.* `helm install` returns once the chart's pre-install hook
  Job has run: 2.1 s on litek8s, 5.8 s on k3s (Job controller → scheduler →
  kubelet → containerd → pause + container, each a separate loop). The rest
  is the seven pods starting; litek8s starts a pod with `unshare`, one `ip
  -batch` and `crun create/start` per container, k3s goes through
  containerd, a pause container and runc.
- *The one slower step.* The first Application sync includes pulling the
  example app's image (`gcr.io/google-samples/gb-frontend:v5`): 18.1 s on
  litek8s against 15.7 s on k3s, with the same clone in the repo server, so
  most likely litek8s's image pull and unpack. Not investigated further.
- *Drift.* Self-heal is driven by Argo CD's watch on the Deployment, so
  recreation takes 0.3–0.4 s on both; becoming Available again is faster on
  litek8s because its pod start is (0.4 vs 1.1 s). One k3s repeat hit Argo
  CD's self-heal backoff (7.2 s).

**What Argo CD broke in litek8s, fixed on the way** (each with a test;
`cargo test` passes with and without `--no-default-features`, and the
101-case compat corpus passes, 101/101 on crun, x86_64):

- *Syncs never finished.* Argo CD 3.x clears `operation` with a JSON patch
  that adds `null`. kube-apiserver treats a null in a non-nullable custom
  resource field as absent (default or drop); litek8s rejected it with 422,
  so every sync stayed "Running" (`994df55`).
- *Watch storm after an API server restart.* A watch from a resourceVersion
  older than the history must end with 200 and an ERROR event (410
  Expired), not a 410 response: client-go's RetryWatcher (Argo CD's cluster
  cache) only relists on the event, and retried the old version ~50 times a
  second forever (`bb3e494`).
- *Deleting an Application hung.* Argo CD deletes resources in the
  foreground; the Service controller recreated the EndpointSlice the garbage
  collector had to delete first, so the Service never went away. Foreground
  deletions under way at a restart were also never resumed (`36325e8`).
- *Memory under many watches.* A resumed watch parsed the whole event
  history (every resource type) into a backlog it kept for its lifetime: 54
  of 72 MB of heap under Argo CD (heaptrack). Each custom resource read,
  write and watch event re-parsed its CRD, a few MB for Argo CD's
  Application. glibc gave each busy thread its own arena: 470 MiB RSS on a
  16-thread host, ~120 MiB with two (`bb3e494`, `46ec8de`, `47935cb`).
- *Events never expired.* kube-apiserver keeps them for an hour; litek8s
  kept them forever (3,700 after five hours of Argo CD) (`694ef91`).
- *Image pulls.* ECR Public rate-limits anonymous pulls with 429; the pull
  failed into the pod's backoff and Argo CD's Redis took two minutes to
  start. 429/503 are now retried in place (`3b2bb89`). A failed layer unpack
  left its partial tree behind on every retry (`6e740e5`, also fixed on main).
- *A corpus regression from upstream:* the current kyverno chart renders
  `replicas:` with no value; Kubernetes decodes that as unset (default 1),
  litek8s kept the null and Helm's readiness check crashed (`cbffdd2`).
- New: `/metrics` also serves `apiserver_request_total` and
  `apiserver_longrunning_requests` with kube-apiserver's labels (`5977b36`),
  which is how the request rates above compare.

The heap work overlapped with main's own (compact JSON objects, byte-bounded
history, periodic `malloc_trim`, merged in `d354f98`); together they took
the API server under Argo CD from 129 to 72 MiB PSS in the Mac dry runs
(same 2 GiB shape, arm64).

Caveats: one guest shape and one host; the CPU percentages are of a Ryzen
5800X core. The sync-window request rates include the driver's own
`kubectl` runs that are not single GETs (24 per round on both systems).
Earlier dry runs on the Mac (arm64 guests under HVF, 2 vCPU / 2 GiB, short
windows; the example apps' amd64-only image ran under qemu-user there) gave
the same picture: k3s-server 681 MiB with Argo CD, litek8s 72 MiB.

## Observability: built in vs kube-prometheus-stack + Loki (2026-10-20, x86_64/KVM)

What does it cost to see a small cluster's metrics and logs and get alerts?
The usual answer on k3s is kube-prometheus-stack (Prometheus, Alertmanager,
the operator, kube-state-metrics, node-exporter) plus Loki and a log
shipper; litek8s has it built in ([docs/observability.md](../docs/observability.md)).

**Setup.** `bench/run.sh obs-k3s obs-litek8s`, both at once on omarchy (AMD
Ryzen 7 5800X, 16 threads; qemu + KVM in a Docker container given
`/dev/kvm`). Each system gets a fresh Ubuntu 24.04 amd64 guest of **2 vCPU /
3 GiB**, no swap, and the same workload ([`obs/workload.yaml`](obs/workload.yaml):
three idle nginx pods and two log generators writing 10 lines/s each, one in
ten an ERROR on stderr). k3s v1.36.5+k3s1 minimal
(`--disable=traefik,servicelb,metrics-server`; local-path kept for Loki's
volume) with **kube-prometheus-stack 92.10.1** (chart defaults: Prometheus
with a 30 s scrape interval and 10 d retention, Alertmanager, operator,
kube-state-metrics, node-exporter, the default rules; Grafana off, since
litek8s has none either, and the controller-manager/scheduler/etcd/proxy
jobs off, which k3s does not expose), **Loki 7.3.0** (3.6.12, single binary,
filesystem storage, no caches or gateway: [`obs/loki-values.yaml`](obs/loki-values.yaml)) and
**Alloy 1.14.0** tailing `/var/log/pods` ([`obs/alloy-values.yaml`](obs/alloy-values.yaml)),
against `litek8s apiserver --runtime crun` (crun 1.14.1) at `215253f` with
its defaults (10 s sampling, tiers 10s:6h,1m:7d,10m:90d). Each guest first
runs the workload alone (5 min measured after 60 s), then with
observability: 15 min warm-up, a 60 min measured window (memory at both
ends, CPU over it, store growth on disk, samples ingested), then 20 runs
of each query. The host was otherwise lightly loaded (about 10 % of its
CPU busy during the window, all VMs included). A second k3s round (10 min
warm-up, 30 min window, after fixing the disk probe) and an A/B of litek8s
(two identical 2 vCPU / 2 GiB guests, observability on vs
`--no-observability`, same uptime, 20 min settle, 15 min window) complete
it. Raw data: [`results/2026-10-20-obs-x86-kvm/`](results/2026-10-20-obs-x86-kvm/)
(`report.md` per round, `ab-litek8s-obs-{on,off}.json`).

| | k3s + kube-prometheus-stack + Loki + Alloy | **litek8s, built in** |
|---|---|---|
| Memory observability adds (PSS) | **1,371 MiB** (round 2: 1,406) | **+1.8 MiB** (A/B: 32.0 vs 30.2 MiB API server) |
| … its own pods | 1,057 MiB (Prometheus, Loki, Alloy, operator, KSM, node-exporter, Alertmanager) | – (in the API server and node agent) |
| … orchestrator growth (k3s-server serving scrapes and watches, containerd) | +314 MiB (round 2: +386) | (included above) |
| Guest memory in use, workload alone → with observability | 842 → 1,739 MiB of 2,960 | 338 → 350 MiB (A/B) |
| CPU observability adds, % of one vCPU | **4.2 %** (round 2: 4.1 %) | **0.03 %** (A/B: 0.042 vs 0.010 % API server) |
| Installed (images; binary) | 2.1 GiB of images | 0 (+2.1 MiB of stripped binary) |
| Series kept | 69,672 (round 2: kubelet/cAdvisor 37.2 k, apiserver 27.0 k, node-exporter 1.1 k, KSM 1.2 k, the stack itself 3.0 k) | 342 |
| Samples ingested / s | 2,349 (30 s scrape) | 34.1 (10 s sampling) |
| Metrics on disk | WAL +45–50 MiB/h; blocks ≈ 0.26–0.41 GB/day at Prometheus's 1–2 bytes/sample¹ → **2.0–4.1 GB** for its default 10 days | 1.60 bytes/sample measured; **≈ 10 MB for 90 days** of this workload² |
| Logs on disk, beyond the CRI files both keep (18–19 MiB/h here) | Loki chunks +2.2 MiB/h (+ its WAL) → ≈ 52 MiB/day, 365 MiB at 7 days | **0** (search reads the CRI files; removed containers' logs kept up to 512 MiB/node) |
| Metric query, instant (7 dashboard queries) | 0.3–0.5 ms | 0.2–0.4 ms |
| Metric query, 1 h range at 15 s | 0.4–1.5 ms | 0.8–4.3 ms |
| Log query: ERRORs of `app=loggen`, last hour, 1,000 lines | 49 ms (round 2: 29) | 33 ms |
| Log query: "timeout" in every pod, last 15 min, 1,000 lines | 20 ms (round 2: 15) | 32 ms (before bisecting to the time range; see below) |
| Restarts, OOM kills | 0, none | 0, none |

¹ A 60 min window does not reach Prometheus's first block (it cuts one when
its head spans 3 h); its head chunks on disk grew only 131 KiB in round 2
while the WAL grew 22.6 MiB in 30 min, so the compressed size is estimated
from the samples ingested and the 1–2 bytes per sample Prometheus documents.
² Measured: the raw tier grew 192 KiB in the hour (1.60 bytes/sample); the
1-minute tier 45 KiB for its first downsampled hours (≈1.1 bytes/point).
Steady state with the default tiers: 6 h raw ≈ 1.2 MB, 7 d at 1 min ≈ 3.8 MB,
90 d at 10 min ≈ 4.9 MB.

**Read:** on a small node, the usual stack is the biggest thing running:
1.4 GiB of memory (more than k3s itself, nearly half the 3 GiB guest), 4 %
of a vCPU and 2 GiB of images, before Grafana. litek8s's built-in metrics,
logs and alerts cost under 2 MiB and a few hundredths of a percent of a
vCPU, and take no extra disk for logs.

**What is not equal.** The two do not collect the same thing, and the table
should be read that way:

- kube-prometheus-stack keeps **200× more series**: every cAdvisor and
  kubelet metric (37 k), kube-apiserver's per-resource latency and size
  histograms (27 k), per-CPU node metrics, and its own components'. litek8s
  keeps a curated set (the [list](../docs/observability.md#what-is-collected)):
  per-container CPU, memory (anon/cache/working set), I/O, OOM kills,
  throttling, network per pod, node CPU/memory/filesystem/network/disk,
  kube-state-metrics' main families, its own request metrics. That covers
  the usual Kubernetes dashboard panels and the built-in alerts, not
  everything Prometheus can answer. litek8s also samples 3× more often
  (10 s vs 30 s).
- Prometheus scrapes any application's `/metrics`; litek8s does not scrape
  applications yet.
- Loki indexes labels and stores compressed chunks that outlive the node's
  CRI files and pod deletions for its retention; litek8s searches the CRI
  files (rotated at 10 MiB per container) and keeps logs of removed
  containers in a per-node archive (7 days, 512 MiB by default). Searching
  days of a chatty cluster's logs is a file scan on every node (in
  parallel), not an index lookup.
- litek8s's range queries are slower (0.8–4.3 ms vs 0.4–1.5 ms) but far
  below what a dashboard notices; its instant queries are as fast.
- Disk: Prometheus at its default 10 days is gigabytes because of the
  series count; litek8s downsamples (1 min after 6 h, 10 min after 7 days),
  so old data answers at a coarser resolution.

**Log search, after the run.** The 15-minute search read each container's
whole log file (up to 10 MiB + the rotated one) to find the last 15 minutes.
Since `d1c22ac` the agent bisects each file to the first line in range
(records are in time order); re-measured on the same workload at the same
uptime: see the follow-up note below.

**Rerun:** `LITEK8S_BIN=<linux binary> bench/run.sh obs-k3s obs-litek8s`
(`OBS_WARMUP_SECS`, `OBS_WINDOW_SECS`, `OBS_QUERY_RUNS`, `OBS_VM_MEM`), then
`python3 bench/obs_report.py bench/results/<timestamp>`.

## Remote node over SSH (litek8s only)

Control plane in one guest, the node in a second guest of the same shape, reached through
`ssh` with ControlMaster exactly as `SshNodeClient` does it.

| RPC path | mean | p95 | CPU on control plane | CPU on node |
|---|---|---|---|---|
| `ssh` mux running `true` (transport floor) | 4.99 ms | 5.52 ms | 4.4 ms | 2.2 ms |
| `ssh` mux + `litek8s agent` exec, `Ping` | 7.22 ms | 8.04 ms | 4.4 ms | 4.8 ms |
| `ssh` mux + `litek8s agent` exec, `ListContainersForDeployment` | 7.36 ms | 8.09 ms | 4.6 ms | 5.0 ms |
| same requests straight to the resident `agent --serve` socket | **0.23 ms** | 0.32 ms | – | – |

RPC census with 5 nginx deployments on the SSH node, steady state (nothing changing), 60 s:

| request | per second | loop |
|---|---|---|
| `StatsSnapshot` | 7.50 | cluster snapshot (1 per deployment per refresh) |
| `ListContainersForDeployment` | 2.58 | reconcile (1 per deployment per pass) |
| `ProbeContainerTcp` | 2.52 | reconcile (1 per deployment per pass) |
| `Ping` | 1.62 | cluster snapshot (1 per node per refresh) |
| `ListManagedContainers` | 0.50 | reconcile (orphan cleanup, 1 per node per pass) |
| `HostResources` | 0.18 | cluster snapshot (5 s cache) |
| **total** | **14.9 RPC/s** | **11.2 RPCs per reconcile pass**, one pass every 2.0 s |

That traffic costs **13.3 % of a vCPU on the node** (vs 5.8 % for the same 5 deployments on
litek8s's local node) and **7.8 % on the control plane**, 5.8 % of it in forked `ssh` clients.

## Biggest litek8s overheads, in order

1. **Docker is about 90 % of the footprint.** At idle dockerd (73 MiB) and containerd
   (44 MiB) are ten times the size of litek8s itself (11 MiB), and the docker.io packages
   are 254 of the 285 MiB on disk. dockerd sits on top of containerd and is pure overhead:
   dropping it alone would save 73–89 MiB, as much as k3s's whole embedded containerd
   (52–105 MiB). Replacing dockerd + containerd with crun/litecontainer is the largest single
   win for memory and disk.

2. *(Fixed in `cfb4222`, see above.)* **Polling makes the runtime work 15× harder.** Going from 0 to 5 deployments takes
   dockerd + containerd + shims from 0.34 % to 5.27 % of a vCPU, while litek8s serve only goes
   from 0.47 % to 0.59 %. litek8s causes that runtime load itself:
   - The cluster snapshot loop calls `StatsSnapshot` (`docker stats`) for **every deployment
     on every refresh**. It refreshes on its 1 s ticker *and* after every reconcile pass, so
     about 1.6 times a second, or 7.5 stats calls/s for 5 deployments, half of all RPCs.
     Containerd's `write()` traffic, mostly socket IPC, was 12.6 MiB/min in that window.
   - Every 2 s pass lists containers and inspects + probes each deployment, even when nothing
     has changed.

   All of it is O(deployments) per second, with no change needed to trigger it. Cheap fixes:
   collect stats only while `top`/snapshot clients are actually watching, drop the
   post-reconcile snapshot refresh, do one list per node per pass instead of one per
   deployment, and back off the probe once a replica is Ready. Event-driven is better still.

3. **SSH fork/exec per remote RPC: 31× the latency, about 9 ms of CPU per call.** Each call
   forks `ssh` on the control plane (about 4.4 ms of CPU). On the node it costs an sshd
   session (2.2 ms) plus an exec of the 23 MiB `litek8s agent` proxy (about 2.6 ms more),
   which then forwards to the resident `agent --serve` that already exists. The resident
   socket answers the same request in 0.23 ms vs 7.2 ms. Calls in a reconcile pass are
   **sequential**, so a pass costs 11.2 × (RTT + ~7 ms). That is about 80 ms here; with 5
   deployments over a 50 ms WAN it is about 0.6 s, and somewhere around 30–40 deployments it
   no longer fits the 2 s tick (an estimate from the measured per-call cost, not measured).
   The resident agent and its TCP listener already exist; a persistent connection, or a node
   push, removes the ssh + exec entirely.

4. *(Mostly fixed in `cfb4222`: 1.9 RPCs per pass, no longer O(deployments) except probes every 10 s.)*
   **The RPC count grows with deployments, even at steady state.** That is 2 RPCs per deployment
   per reconcile pass plus 1 per deployment per snapshot refresh. One batched "node status"
   call (all containers + probe results + stats) would make it about 1 per node per pass.

5. **state.json is not an idle cost.** In the 5-minute steady-state window litek8s serve
   wrote **0 bytes**: the store persists only when something changes. The file is 30 KB with
   5 deployments, against 9–19 MB of k3s SQLite plus 1.6–2.6 MiB/min of `write()` traffic from
   k3s-server. The full pretty-printed rewrite (including up to 250 events) will cost during
   churn: restarts, rollouts, flapping probes. This benchmark does not exercise churn.

6. *(Fixed in `cfb4222`.)* *Minor:* each pass proposes `SetReplicaAssignment` + `SetReplicaStatus` through Raft for
   every replica, even when they are unchanged. The log is in memory and compacted by
   openraft's default snapshot policy, and all of litek8s serve stays under 0.6 % CPU, so it is
   low priority.

## Method and caveats

- **Host:** guests run under qemu + HVF on an Apple M3 Max. The memory and disk numbers carry
  over to a real t4g.micro. **The CPU percentages will be higher on Graviton2**, whose cores
  are slower (expect roughly 2–3×; not verified), but the ratios between systems should hold.
  To rerun on real instances, point `BENCH_HOST_<name>` at fresh hosts (see `vm.sh`). No
  cloud resources were created for this run.
- **Versions:** litek8s `78fc083` (release build, linux/arm64), docker.io
  29.1.3-0ubuntu3~24.04.2 (Ubuntu's package, same as `infra/terraform/userdata.sh`),
  k3s v1.36.5+k3s1 (stable channel on 2026-10-16).
- **Memory** is the sum of PSS (shared pages split fairly) per process. Processes are grouped
  by cgroup and command name: control plane = `litek8s serve` / `k3s-server` (k3s's single
  process also holds the apiserver, scheduler, kubelet, kube-proxy and kine/SQLite); runtime =
  dockerd/containerd/shims/pause; addons = non-nginx pods. "MemAvailable" is the kernel's
  estimate and treats file-backed pages (Go binary text) as reclaimable, so litek8s's drop in
  MemAvailable at idle (56 MiB) is smaller than its PSS (128 MiB).
- **CPU** is cumulative cgroup `cpu.stat` (so short-lived children such as ssh, iptables and
  agent execs are counted) plus per-process ticks, over a 300 s window that starts 60 s after
  the system settles. "Whole guest busy" is everything, kernel included.
- **Time to ready:** clock starts before the CLI `apply` (`litek8s apply` / `k3s kubectl
  apply`) and stops when the system reports the replica Ready (litek8s socket poll every 50 ms;
  for k3s, a `kubectl get -w` started before the clock). The image is pre-pulled; pull time
  is excluded. k8s uses `periodSeconds: 2` to match litek8s's 2 s tick; its bimodal
  1.7 s / 3.1 s times are consistent with a first probe that misses by one period. litek8s probes on the
  Docker start event, which is part of why it is faster.
- **Fairness tweaks:** the k8s manifests set `requests: 50m/32Mi`. Without them, requests
  default to the limits and 5 × 500m do not fit on 2 vCPU; litek8s does no CPU admission.
  No hostPorts, so no docker-proxy processes.
- **Remote RPC:** the guests reach each other through qemu user-mode networking on the
  same host, so there is no real network RTT. The per-call CPU and fork/exec costs are what
  this measures; a WAN adds its RTT to every call. The census uses a `/bin/sh` shim as `--remote-binary`. It is a plain `exec`
  except while a flag file exists; the CPU window ran with the flag off.
- Each window was run once. There are no repeat statistics on the idle numbers.

## Rerun

```bash
bench/run.sh                        # all phases on fresh local VMs, ~55 min
bench/run.sh litek8s rpc            # just litek8s single-node + remote-node phases
bench/run.sh kube kube-crun         # the Kubernetes path on Docker and on crun
bench/run.sh argocd-k3s argocd-litek8s   # Argo CD on both (ARGO_VM_MEM, ARGO_RUNS)
bench/run.sh obs-k3s obs-litek8s     # observability: k3s + kube-prometheus-stack + Loki vs built in
python3 bench/argocd_report.py bench/results/<run1> bench/results/<run2>   # pooled
IDLE_SECS=60 SETTLE_SECS=10 bench/run.sh k3s-minimal   # quick look
python3 bench/report.py bench/results/<timestamp>      # tables from raw JSON
```

Needs Homebrew `qemu` (aarch64 + edk2 firmware) and Docker (to cross-build the linux binary,
same `rust:1.90` image as `node add --install`). It downloads the Ubuntu 24.04 arm64 cloud
image once into `bench/.work/`. Files: `vm.sh` (throwaway VMs, or attach to real hosts),
`run.sh` (phases), `node/probe.py` (in-guest sampler, time-to-ready and RPC timing),
`report.py` (tables), `build-linux.sh` (cross-build).
