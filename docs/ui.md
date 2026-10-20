# `litek8s ui`: the dev environment in a terminal

`litek8s ui` is litek8s's answer to OrbStack's and Docker Desktop's windows:
see, configure and operate the local dev environment without remembering
commands. It is part of the same binary, keyboard-driven, fits an 80x24
terminal, and works the same locally and over SSH:

```bash
litek8s ui                  # this machine's dev clusters
litek8s ui --cluster ci     # starting on the local cluster `ci`
ssh -t box litek8s ui       # another machine's, in your terminal
```

Nine views, `1`-`9` (or `tab`) to switch, `?` for the keys of each:

| view | what it gives you |
|---|---|
| **1 Home** | the shown cluster's state (up, down, starting, stopping), mode, uptime; CPU, memory and disk of the VM or rootless sandbox; the Docker socket and API server; whether `docker` and `kubectl` point at it. `u` up, `d` down, `R` delete (asks first) |
| **2 Clusters** | the local clusters (state, API port, nodes, memory, age) and your kubeconfig's other contexts; `n` new (name, nodes), `u`/`d` up/down, `D` delete, `enter` shows it in every view (and offers to point `kubectl` and `docker` at it); see [Clusters](#clusters) |
| **3 Containers** | `docker ps -a`, grouped by compose project: `l` logs (follow), `s` a shell in the container, `u`/`x`/`r` start/stop/restart, `d` remove; on a project row they act on the whole project, `space` folds it |
| **4 Images** | sizes, age, which containers and pods use each; `d` remove |
| **5 Volumes** | sizes, which containers mount each, compose project; `d` remove |
| **6 Free space** | what can be reclaimed: stopped containers, dangling images, build cache, unused images, unused volumes; select with `space`, `p` frees it |
| **7 Kubernetes** | a cluster UI for any cluster or context (litek8s, k3s, EKS, GKE, …), k9s-style: `:` any resource (CRDs too), `/` filters and selectors, describe, YAML, edit with a server dry run and a diff, logs of pods and whole workloads, shells, port-forwards, xray trees; see [Kubernetes](#kubernetes-a-cluster-ui) |
| **8 Settings** | CPUs, memory, shared directories, API port, where published ports listen; switch the docker and kubectl contexts to litek8s and back; edit the settings file in `$EDITOR` |
| **9 Observe** | the cluster's alerts and its pods by CPU or memory with sparklines of the last 30 minutes, from the API server's built-in metrics; every log of a pod (gone containers included), a regex search of every pod's logs, acknowledge alerts; see [Observe](#observe) and [observability.md](observability.md) |

Everything that deletes or stops asks first, and says what goes: removing a
compose project lists its containers, deleting a cluster says it deletes
every object, image and volume and where they are, freeing space adds up
what it frees.

## Screens

Captured from real sessions on the macOS VM and, over SSH, the Linux
rootless sandbox; the test runs used scratch paths and a second API port,
shown here as a default install has them (`~/.litek8s`, `~/.kube/config`,
6443).

Home:

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
┌ Cluster dev ─────────────────────────┐┌ Resources ───────────────────────────┐
│● up  macOS VM, up 22m 50s            ││CPU             1% of 4 CPUs          │
│API      http://127.0.0.1:6443        ││Memory ████  466 MB of 4.1 GB VM limit│
│Docker   ~/.litek8s/docker.sock       ││Disk      269 MB used, 131 GB free    │
│Engine   API 27.5.1 (aarch64)         ││Ctrs   3/3 running   Pods 4/4         │
│VM IP    192.168.64.21                ││Images 4  (200 MB)                    │
└──────────────────────────────────────┘└──────────────────────────────────────┘
┌ Your tools point at ─────────────────────────────────────────────────────────┐
│✗ docker context   default                                                    │
│  DOCKER_HOST      unset                                                      │
│✓ kubectl context  litek8s-dev  (~/.kube/config)                              │
└──────────────────────────────────────────────────────────────────────────────┘
 Switch docker/kubectl to litek8s in Settings (8). Help: ?

 u up  d down  R delete  b breakdown  2 clusters                         ? help
```

The memory gauge reads *used of limit*: on the Mac the total is the VM's
`--memory` (`dev up --memory`, Settings), the most the VM may ever use, not
memory set aside: macOS gives the VM pages only as the guest touches them
(and the VM's dynamic ceiling keeps the guest below it while it does not
need more); on Linux it is the sandbox's limit when one is set, else the
machine's RAM. The memory breakdown repeats
it as its last line ("VM limit (--memory) … a ceiling, not reserved").

Under it, Home explains its figures: what the Resources box's memory, CPU
and disk are made of, one box at a time on 80x24 (`b` shows the next; a
tall terminal shows all three). Memory, on the Mac (a fresh VM running the
compat corpus):

```
┌ Memory · 589 MB (what macOS charges; VM ceiling 1.0/4.0 GiB)  b: next ───────┐
│litek8s (dev)           11.9 MB   API server, controllers, node agent         │
│pods & containers        635 kB                                               │
│Linux kernel + base      214 MB   fixed cost: kernel, page tables             │
│cache                    296 MB   reclaimable                                 │
│freed, still charged    65.8 MB   macOS keeps it until the VM restarts        │
└──────────────────────────────────────────────────────────────────────────────┘
```

| line | what it is | from |
|---|---|---|
| litek8s (cluster) | the shown cluster's own processes (API server, controllers, node agent, crun shims) on all its nodes | its cells' cgroups: usage less file pages, less the pods' |
| pods & containers | its pods, Docker containers and image builds | the `pod*`, `docker/*`, `build/*` cgroups under each node |
| other clusters | the same two for every other running cluster, together | their cells |
| Linux kernel + base (macOS) | what the VM costs before anything runs: memory the kernel sets aside at boot (its image and the page structures, ~3.5 % of the VM's size), its unreclaimable slab, stacks and page tables, and tmpfs (an older guest's initramfs, the root filesystem held in RAM); also the VM's own init, cell manager and DHCP client | the VM's size less `MemTotal` and the balloon, `/proc/meminfo` |
| sandbox (Linux) | the supervisor, pasta and the cell manager | the sandbox's cgroup less the clusters' |
| cache | file pages: images, logs, the binary; the kernel drops them under pressure | `/proc/meminfo` (VM) or the sandbox's `file` pages |
| freed, still charged (macOS) | pages the guest used once and has freed (or handed to the balloon since): Virtualization.framework keeps charging them to the VM until it stops, however much macOS needs memory (it compresses or swaps them like any other); the guest reuses them before it touches more. Restarting the VM (`dev down`, `dev up`) hands them back | the footprint less what the guest uses |

The title also says how much of its memory the guest may use now when that
is less than all of it: the VM's ceiling, which grows on demand and shrinks
when the guest is idle ([dev.md](dev.md#memory-on-macos)); what is above it
was never touched, and macOS charges nothing for it.

The headline figure is what macOS charges the VM process (its physical
footprint, what Activity Monitor shows) or, on Linux, the sandbox's cgroup.
The guest's numbers come from its cgroups and `/proc/meminfo` through the
VM's control channel; a remote litek8s (`--context`) answers
`GET /litek8s/v1/memory` on its API server with its node's split (litek8s,
pods and containers, cache) and its meminfo, which Home shows instead. It
is sampled with the rest of Home: every 2 s while Home is on screen, not
otherwise.

CPU, between the last two samples (2 s), in % of one CPU, on the Mac with
a busy-looping pod:

```
┌ CPU · 100.4% of one CPU = 25% of 4 CPUs (the VM process)  b: next ───────────┐
│litek8s (dev)              0.0%   API server, controllers, node agent         │
│pods & containers        100.0%                                               │
│  pod default/burn       100.0%                                               │
│other clusters (1)         0.0%   litek8s and pods of the rest                │
│guest kernel & other       0.3%   interrupts, network, the VM's init          │
│VM overhead                0.1%   virtual devices, vCPU exits (macOS side)    │
└──────────────────────────────────────────────────────────────────────────────┘
```

| line | what it is | from |
|---|---|---|
| litek8s (cluster) | the shown cluster's own processes on all its nodes | its cells' `cpu.stat` less its pods' |
| pods & containers, and the top 3 | every pod, Docker container and build of the cluster, the busiest by name | the `pod*`, `docker/*`, `build/*` cgroups (`usage_usec`) |
| other clusters | the rest of the clusters together | their cells |
| guest kernel & other (macOS) | busy time the guest does not account to a cluster: interrupts, the network stack, the VM's init | the guest's `/proc/stat` less the clusters |
| VM overhead (macOS) | what the VM process ran beyond the guest's busy time: virtual devices, vCPU exits | the VM process's CPU time (macOS) less the guest's `/proc/stat` |
| sandbox (Linux) | the supervisor, pasta and the cell manager | the sandbox's cgroup less the clusters' |

The title's figure is the VM process's CPU time as macOS counts it, or the
sandbox's cgroup; the Resources box says the same as `100% of 1 CPU = 25%
of 4`.

Disk, on the Mac after a container wrote 20 MB and a volume 50 MB and both
were removed (the walk is at most a minute old):

```
┌ Disk · 306 MB (litek8s and its VM disk)  b: next ────────────────────────────┐
│litek8s itself           143 MB   binary, VM kernel and initramfs             │
│cluster dev             78.0 MB                                               │
│  images                77.2 MB   layers shared by pods, Docker, builds       │
│  container layers       250 kB   what containers wrote                       │
│  volumes               81.9 kB   Docker volumes, emptyDirs, PVs              │
│  logs                   8.2 kB   pods and containers, 10 MiB each            │
│  objects                172 kB   Kubernetes objects, keys                    │
│  other                  254 kB   metadata, IP allocations                    │
│rest of the VM disk      2.3 MB   other clusters, filesystem                  │
│deleted in the VM       83.2 MB   disk.img gives it back within a minute      │
└──────────────────────────────────────────────────────────────────────────────┘
```

| line | what it is |
|---|---|
| litek8s itself | the `litek8s` binary; on macOS also the VM's kernel, root image and initrd and the signed copy of the binary the VM runs from |
| images | the unpacked layers of every image and of the build cache: one content-addressed store per node, shared by pods, Docker and builds, each layer once |
| container layers | what pod containers and Docker containers wrote (their overlay upper directories) |
| volumes | Docker volumes (named and anonymous), pods' emptyDirs, local PVs, ConfigMap and Secret files |
| logs | pod and container logs (10 MiB and one rotated file each) |
| builds | builds in progress and `RUN --mount=type=cache` directories |
| objects | Kubernetes objects (the write-ahead log, compacted) and the cluster's keys |
| other nodes | a multi-node cluster's workers (each with its own image store) |
| other | metadata, IP allocations, staging |
| rest of the VM disk (macOS) | the other clusters and the filesystem's own blocks |
| deleted in the VM (macOS) | blocks the guest freed that `disk.img` still holds: the guest trims its disk once it has been quiet for 30 s (at most once a minute), and Virtualization.framework punches the trimmed blocks out of the sparse image, so this goes back to about zero shortly after a prune |

Where images, container layers and volumes have something to reclaim, the
note says how much and points at the Free space view (6), which frees it:
those amounts come from the Docker API's `/system/df` (unused images and
build cache, stopped containers, unused volumes). The categories come from
`GET /litek8s/v1/disk` on the cluster's API server: one walk of the node's
data directory that counts allocated blocks and hard links once, classifies
each file by where it is (so the parts add up to the whole), and runs at
most once a minute, in the background (Home asks every 30 s while it is on
screen). `/system/df`'s `LayersSize` now counts each layer once and images
report their `SharedSize`, as Docker does.

Against a remote litek8s (`--context`), the three boxes show its API server's
node: `GET /litek8s/v1/memory`, `/litek8s/v1/cpu` (cumulative counters, two
samples make the percentages) and `/litek8s/v1/disk`, and its machine's
`/proc/stat` for the rest of its CPU.

After `u` on a stopped cluster, the output of `litek8s dev up` shows under
it (here on Linux, over SSH):

```
┌ Output ──────────────────────────────────────────────────────────────────────┐
│$ litek8s dev up                                                              │
│litek8s-dev ready in 0.10s (rootless, http://127.0.0.1:6443, kubectl context  │
└──────────────────────────────────────────────────────────────────────────────┘
```

On Linux the mode is `rootless sandbox`; with a CPU or memory limit set
(Settings), the gauges are against the limit (a 4-thread busy loop under a
2-CPU limit):

```
┌ Cluster dev ─────────────────────────┐┌ Resources ───────────────────────────┐
│● up  rootless sandbox, up 20s        ││CPU    ████████200% of 2 CPUs ████████│
│API      http://127.0.0.1:6443        ││Memory █      54.8 MB / 2.1 GB        │
```

Containers, with a compose project and a confirmation:

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
┌ Containers (3/3 running) ────────────────────────────────────────────────────┐
│NAME                      STATE     IMAGE                   PORTS             │
│▾ shop                    3/3 up    compose project                           │
│  cache                   running   redis:alpine            6379/tcp          │
│  web                     running   nginx:alpine            18080->80/tcp     │
│  worker                  running   busybox                                   │
│                                                                              │
│                     ┌ Stop shop-worker-1? ─────────────┐                     │
│                     │shop-worker-1 (busybox, running)  │                     │
│                     │                                  │                     │
│                     │y yes   n/esc no                  │                     │
│                     └──────────────────────────────────┘                     │
└──────────────────────────────────────────────────────────────────────────────┘
 l logs  s shell  u start  x stop  r restart  d remove  / filter         ? help
```

`l` follows a container's (or pod's) log; `↑`/`PgUp` scroll back and stop
following, `G` or `f` follow again, `w` wraps long lines. Escape sequences
are stripped, so colored logs cannot paint over the UI:

```
┌ logs: shop-worker-1  [follow] ───────────────────────────────────────────────┐
│tick 110                                                                      │
│tick 111                                                                      │
│tick 112                                                                      │
└──────────────────────────────────────────────────────────────────────────────┘
 esc back  f follow  w wrap  ↑↓ scroll  g/G top/end                      ? help
```

`s` suspends the UI and opens a shell in the container (bash if it has one,
else sh) with your terminal's size, following resizes; exit the shell to
come back. It is `docker exec -it` over the Docker API, so it needs no
`docker` CLI.

Images:

```
┌ Images (4, 200 MB) ──────────────────────────────────────────────────────────┐
│IMAGE                           SIZE     AGE   USED BY                        │
│alpine:latest                   8.7 MB   31d   unused                         │
│busybox:latest                  4.2 MB   159d  shop-worker-1 +1               │
│nginx:alpine                    68.5 MB  4d    shop-web-1 +3                  │
│redis:alpine                    119 MB   27d   shop-cache-1                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

Free space on Linux after two `docker build`s, then freeing stopped
containers and unused images on the Mac:

```
┌ Reclaimable space ───────────────────────────────────────────────────────────┐
│    WHAT                                                      ITEMS  SIZE     │
│[x] Stopped containers                                        1      0 B      │
│[x] Dangling images (untagged, unused)                        0      0 B      │
│[x] Build cache                                               2      5.2 MB   │
│[ ] Unused images (no container or pod)                       2      23.1 MB  │
│[ ] Unused volumes (data is lost)                             0      0 B      │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
 Selected: 5.2 MB   p frees it (asks first)

 litek8s data  73.7 kB in ~/.local/share/litek8s/dev
 disk free    1.5 TB
```

```
│[x] Stopped containers                                        2      0 B      │
│[x] Dangling images (untagged, unused)                        0      0 B      │
│[x] Build cache                                               0      0 B      │
│[x] Unused images (no container or pod)                       2      11.8 MB  │
│[ ] Unused volumes (data is lost)                             0      0 B      │
│             ┌ Free space? ─────────────────────────────────────┐             │
└─────────────│Stopped containers: 2 (0 B)                       │─────────────┘
 Selected: 11.│Unused images (no container or pod): 2 (11.8 MB)  │
              │About 11.8 MB freed. This cannot be undone.       │
              │                                                  │
              │y yes   n/esc no                                  │
              └──────────────────────────────────────────────────┘
```

Kubernetes, all namespaces (`0`), `ctrl-z` would keep the two crashing pods:

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
 ⎈ litek8s-ui-test  ns all  │ pods
┌ pods(all) [6] ───────────────────────────────────────────────────────────────┐
│ NAMESPACE NAME                   READY STATUS           RESTARTS AGE         │
│ default   web-8567f99496-mtcrn   2/2   Running          0        33m         │
│ default   web-8567f99496-r7hnq   2/2   Running          0        29m         │
│ shop      api-6df8b694fd-ck8sd   0/1   CrashLoopBackOff 1        16s         │
│ shop      api-6df8b694fd-x697f   0/1   CrashLoopBackOff 1        16s         │
│ shop      cache-599d9956dc-r8xlm 1/1   Running          0        16s         │
│ shop      migrate-6lslk          0/1   Completed        0        16s         │
└──────────────────────────────────────────────────────────────────────────────┘
 : cmd  / filter  l logs  s shell  d describe  F fwd  m menu             ? help
```

More of it in [Kubernetes](#kubernetes-a-cluster-ui).

Settings, after changing the shares and where ports listen while the VM
runs:

```
┌ Settings ────────────────────────────────────────────────────────────────────┐
│SETTING              VALUE                                                    │
│VM CPUs              4                                                        │
│VM memory (MiB)      4096                                                     │
│Shared directories   /Users/me, /Users/Shared                                 │
│API port (dev)       6443 (default)                                           │
│Published ports on   all interfaces (LAN)                                     │
│docker context       default  (enter: use litek8s)                            │
│kubectl context      other  (enter: use litek8s-dev)                          │
│Config file          ~/.litek8s/config.yaml  (enter: $EDITOR)                 │
└──────────────────────────────────────────────────────────────────────────────┘
 ⟳ Restart needed for published ports, shared directories: Home (1), d then u.
 CPUs, memory and shares size the macOS VM.
 Settings apply on the next `litek8s dev up` (its flags win).
```

## Clusters

`litek8s dev` runs as many clusters as you like in one VM or sandbox
([dev.md](dev.md#several-clusters)); view 2 lists them, with the VM's or
sandbox's size and what it uses, and the other contexts of your kubeconfig
(a home cluster, a cloud one) read-only. On the Mac, with a 3-node cluster
and a stopped one:

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
┌ Local clusters · shared VM: 4 CPUs, 4.3 GB max, 412 MB used ─────────────────┐
│NAME           STATE     API              NODES MEMORY    AGE  CONTEXT        │
│● dev          up        127.0.0.1:6443   1     27.2 MB   8m   litek8s-dev    │
│  ci           up        127.0.0.1:61325  3     5.6 MB    2m   litek8s-ci     │
│  old          down      -                1     -         3d   litek8s-old    │
└──────────────────────────────────────────────────────────────────────────────┘
┌ Other kubeconfig contexts (read-only; enter: Kubernetes view) ───────────────┐
│  home                          https://192.168.1.10:6443                     │
└──────────────────────────────────────────────────────────────────────────────┘
 All clusters share one VM (its CPUs and memory: Settings).

 n new  u up  d down  D delete  enter use                                ? help
```

`●` marks the cluster the other views show. MEMORY is the cluster's own:
its nodes' processes and pods, counted the way `docker stats` counts a
container (the binary and the VM's kernel are shared and not in it); the
title's figure is the whole VM (or sandbox). NODES shows `2/3` while a node
is not Ready.

`n` asks for a name and a node count, and says what the cluster shares:

```
         ┌ New cluster ───────────────────────────────────────────────┐
         │Name   ci2█                                                 │
         │Nodes  3                                                    │
         │                                                            │
         │Runs in the shared VM: 4 CPUs, 4096 MiB for all clusters.   │
         │Context litek8s-ci2, socket ~/.litek8s/ci2/docker.sock      │
         │Nodes after the first are workers joined like real nodes.   │
         │tab next field   enter create   esc cancel                  │
         └────────────────────────────────────────────────────────────┘
```

and runs `litek8s dev up --name ci2 --nodes 3 --no-switch` (kubectl's
current context stays as it is), its output under the list. `D` says what
goes before it goes (here on Linux):

```
 ┌ Delete cluster t1? ────────────────────────────────────────────────────────┐
 │Stops it and DELETES all of its data:                                       │
 │Kubernetes objects, pods, Docker containers, images, volumes                │
 │and the build cache, in ~/.local/share/litek8s/dev/clusters/t1/data.        │
 │The kubectl context litek8s-t1 is removed. This cannot be undone.           │
 │                                                                            │
 │y yes   n/esc no                                                            │
 └────────────────────────────────────────────────────────────────────────────┘
```

`enter` on a local cluster shows it everywhere (Home, Containers, Images,
Volumes, Free space and Kubernetes follow its Docker socket and API) and
then asks whether `kubectl` and `docker` should point at it too
(`current-context` and the docker context `litek8s-<name>`); the UI
remembers it for next time. `enter` on another context shows that context
in the Kubernetes view only (nothing to start or stop there); `enter` on a
local cluster comes back.

While a cluster or its engine is down, Home says so instead of counting
nothing: `Ctrs –`, `Pods –`, `Images – (engine not running)`, not `0/0` and
`0 (0 B)`.

## Observe

View 9, on the cluster's built-in metrics, logs and alerts
([observability.md](observability.md)):

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
┌ Alerts · 3 firing, 0 pending ────────────────────────────────────────────────┐
│● firing  NodeFilesystemAlmostFull 7m     Node obs-smoke has 4.853% disk left…│
│● firing  KubePodCrashLooping      6m     default/crasher-778d767f48-4zrkx (b…│
│● firing  KubePodRestartLoop       5m     default/crasher-778d767f48-4zrkx (b…│
└──────────────────────────────────────────────────────────────────────────────┘
┌ Pods by CPU, last 30 min ────────────────────────────────────────────────────┐
│NAMESPACE    POD           CPU                      MEMORY                    │
│shop         db-0            800m ▅▅▅ ▅▆▆▆▆▆▇▇▇▇███  60.0 MB ▂▂▂ ▃▃▃▃▃▃▃▃▃▃▃▄▄│
│default      web-7d9-abc     100m ▁▁▁ ▂▂▂▂▂▂▂▂▂▂▂▂▂   160 MB ▅▅▅ ▅▆▆▆▆▆▇▇▇▇███│
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
 l logs  L search logs  s sort  a/A ack  n namespace                     ? help
```

- **Alerts** first: firing (red when critical), pending, acknowledged
  (dim); `a` acknowledges the selected pod's alerts, `A` all of them (each
  silenced until it resolves; asks first).
- **Pods** by CPU (cores, `m` = millicores) or memory (working set; `s`
  switches), top consumers first, with a sparkline per pod over the last 30
  minutes (one cell per ~2 minutes; a gap where there was no sample).
  `n` cycles namespaces, `/` filters.
- **Logs**: `l` opens every log of the selected pod over the last hour,
  following, including containers that restarted or are gone; `L` asks
  for a regex and searches every pod's logs (of the namespace shown) over
  the last hour, then follows.

It reads `/prometheus/api/v1/query_range` (two queries) and
`/litek8s/v1/alerts` every 10 s while the view is on screen, and nothing
otherwise; against an API server with `--no-observability` it says so.

## Settings: `~/.litek8s/config.yaml`

The Settings view edits a small file that `litek8s dev up` reads (its flags
win over it); `$LITEK8S_CONFIG` moves it. Every change is validated before it
is written: CPUs between 1 and the machine's, memory between 1024 MiB and
its RAM, a port of 1024 or above, shared directories that exist, are
absolute, do not overlap and are not already shared.

```yaml
cpus: 4              # macOS: the VM's CPUs (default: half the Mac's, at most 8)
                     # Linux: a CPU limit on everything the dev cluster runs (default: none)
memory: 4096         # MiB; macOS: the VM's ceiling (default 4096); Linux: a limit (default: none)
shares:              # macOS: directories shared with the VM at the same path (default: $HOME;
  - /Users/me        #   /tmp and /var/folders are always shared)
  - /Volumes/src
port: 6443           # the API server's port on localhost
publish: localhost   # where published container ports and NodePorts listen:
                     #   localhost (default) or all (other machines on the network too)
```

The file is read when the cluster starts. The cluster records what it
started with, so the Settings view (and nothing else) tells you which
changes still need `d` and `u` on Home. "Config file" opens it in
`$VISUAL`/`$EDITOR` (`vi` otherwise) and checks it when the editor exits.

## Contexts: pointing your tools at litek8s

Home shows, and Settings switches:

- **docker context.** "use litek8s" creates (or updates) a `litek8s`
  context for `unix://~/.litek8s/docker.sock` (for another cluster,
  `litek8s-<name>` for `~/.litek8s/<name>/docker.sock`) and makes it current, with
  `docker context` when the CLI is installed and by writing the CLI's
  context store (`$DOCKER_CONFIG/contexts`, `config.json`) when it is not.
  "switch back" returns to the context that was current before
  (`default` if none was recorded). `DOCKER_HOST`, when set in your shell,
  wins over any context; Home says so.
- **kubectl context.** "use litek8s-dev" makes the shown cluster's context
  (`litek8s-<name>`) current (adding it if the cluster runs); "switch back" restores the
  previous current context. Nothing else in the kubeconfig changes.

What was current before is kept in `~/.litek8s/ui-state.json`.

## Kubernetes: a cluster UI

View 7 is a cluster UI in the spirit of [k9s](https://k9scli.io), for any
cluster: the local dev clusters, and every context of your kubeconfig
(k3s, kind, EKS, GKE, AKS, …). Its keys are k9s's where k9s has one, so
k9s habits work; it is lighter (see [the numbers](#kubernetes-performance))
and comes with litek8s.

```bash
litek8s ui --context prod -n payments   # start on a context and namespace
```

or `:ctx` inside to pick one. Credentials are what kubectl reads:
`$KUBECONFIG` (all its files, merged as kubectl merges them), tokens, token
files, client certificates, basic auth, and **exec plugins**
(`gke-gcloud-auth-plugin`, `aws eks get-token`, `kubelogin`, …), run when a
token is needed and again when it expires or the server refuses it. Every
request goes to the context the UI shows; `:kubectl` passes it as
`--context`, so the kubeconfig's current context is never used by accident.

### Command mode (`:`)

| command | what it shows or does |
|---|---|
| `:pods`, `:po`, `:deploy`, `:dp`, `:svc`, `:ing`, `:no`, `:ns`, `:ev`, `:crd`, `:cm`, `:sec`, `:pvc`, … | that resource type: any type the cluster serves, custom resources and aggregated APIs included, by plural, singular, short name, kind or `plural.group` (`:certificates.cert-manager.io`, `:cert`) |
| `:pods kube-system`, `:pods -n kube-system`, `:deploy -A` | in a namespace, or all of them |
| `:pods -l app=web,tier!=db`, `:pods -f status.phase=Running`, `:pods /nginx` | with a label selector, a field selector (sent to the server), a filter |
| `:ctx` / `:ctx prod` | pick a context / switch to one (the views reconnect) |
| `:ns` / `:ns web` | the namespaces / switch the namespace |
| `:xray deploy [ns]` | owner trees: deploy → ReplicaSets → pods → containers (also `sts`, `ds`, `rs`, `job`, `cronjob` → jobs → pods, `svc` → its pods, `node` → its pods) |
| `:kubectl get events -A`, `:!top pods`, `!` | kubectl on the UI's context and namespace; its output in a pager (`exec -it`, `edit`, `attach` get the terminal) |
| `:pf` | the running port-forwards |
| `:aliases` | every resource type, to pick one |
| `:help`, `:q` | the keys; quit |

`tab` (or `→`) completes resource names, commands, namespaces after `-n` or
`:ns`, contexts after `:ctx`; the first candidate shows greyed after the
cursor. `↑`/`↓` walk the history (kept in `~/.litek8s/ui-history`).
Aliases of your own come from [`ui.yaml`](#uiyaml).

Columns are the server's: the same Table kubectl prints (custom resources'
printer columns included), `ctrl-w` adds the wide ones (`-o wide`). Ages are
computed from the creation time, so they move on. Rows are colored by
health: pods by their status and readiness, workloads by ready counts,
nodes by conditions, anything with a Status column by its words.

### Filters (`/`)

The list narrows as you type; `tab` switches the mode:

| filter | keeps |
|---|---|
| `web` | rows whose name or any cell contains it (substring, any case) |
| `^web-\d+` | the same as a regex (mode `regex`) |
| `wbf` | fuzzy: the letters in order, tighter matches first (mode `fuzzy`) |
| `!running` | the rows that do *not* match |
| `-l app=web,tier in (a,b),!canary` | rows whose labels match the selector (matched here, instantly) |
| `-f status.phase=Running` | a field selector, sent to the server (a list of its own) |

They combine (`!running -l app=web`). Each resource type keeps its last
filter while the UI runs, and the table's title shows it. `ctrl-z` keeps
only the rows that are not fine (crashing, pending, not ready); `esc`
clears marks, then the filter, then goes back.

Sorting: `<` and `>` pick the column (an arrow marks it), `O` reverses it,
`N` `A` `S` sort by name, age (newest first), status as in k9s. Numbers sort
as numbers, `2/3` by its counts, `512Mi` and `250m` by size.

### Acting on objects

`m` opens the selected row's actions as a menu; each has a key:

| key | on | does |
|---|---|---|
| `enter` | deploy, sts, ds, rs, job, svc, node | its pods (by its selector, or on the node) |
| `enter` | pod / ns / crd / cronjob | its containers / switch to it / its objects / its jobs |
| `d` / `y` | anything | describe (kubectl's, events at the end) / YAML (`m` there shows managedFields) |
| `e` | anything | edit in `$EDITOR` (below) |
| `ctrl-d`, `del` | anything | delete, asking first; `p` there picks the cascade (Background, Foreground, Orphan), `f` no grace period |
| `ctrl-k` | pod | kill: delete with no grace period (asks) |
| `space` | anything | mark; actions apply to every marked row (`ctrl-\` unmarks) |
| `l` / `p` | pod, container, workload, svc | logs / the previous containers' logs |
| `s` | pod, container | a shell (pick the container when there are several) |
| `s` | deploy, sts, rs | scale |
| `r` / `u` / `H` | deploy (`r`: sts, ds too) | rollout restart / undo (to the previous revision) / history and status |
| `F` | pod, svc, deploy, sts, rs | port-forward (a service's or workload's goes to one of its ready pods) |
| `f` | | the port-forwards (`d` stops one) |
| `c` / `u` / `r` | node | cordon / uncordon / drain (evictions, honoring PodDisruptionBudgets; DaemonSet pods stay) |
| `t` | cronjob | run it now (a Job from its template) |
| `x` | secret / workloads | decode its data / its xray tree |
| `o` / `J` / `E` | pod / anything owned / anything | its node / its owner / its events |
| `c` / `C` | anything | copy the name / the YAML (OSC 52, so it works over SSH) |
| `n` / `0` | | pick a namespace / all namespaces and back |

**Edit** works like `kubectl edit`, with the server checking it first: the
object goes to `$EDITOR` as YAML; what comes back is sent as a dry run
(`PUT ?dryRun=All`), so the API server validates it (schemas, admission
webhooks) without changing anything. A mistake comes back into the editor
as comment lines (`# Error: ...`) over your edit; a valid change shows as a
diff, which `y` applies (`e` edits again, `esc` drops it):

```
┌ edit deploy/team-0/web (validated by the server; not applied yet)  y apply · e edit · esc drop ──┐
│@@ -13,7 +13,7 @@                                                                                 │
│   uid: 256ba4d2-d410-42be-99f6-f48045d36606                                                      │
│ spec:                                                                                            │
│   progressDeadlineSeconds: 600                                                                   │
│-  replicas: 3                                                                                    │
│+  replicas: 4                                                                                    │
│   revisionHistoryLimit: 10                                                                       │
```

### Logs

`l` on a pod with several containers asks which (or all of them); on a
deployment, statefulset, daemonset, job or service it follows every
container of every pod it selects (up to `logs.maxStreams`, 30), each with
a colored prefix, interleaved by time:

```
┌ logs deploy/default/web (all pods)  [follow]  4 streams ─────────────────────┐
│mtcrn/ticker ERROR something odd 3000                                         │
│r7hnq/ticker tick 2776 from web-8567f99496-r7hnq                              │
│mtcrn/ticker tick 3001 from web-8567f99496-mtcrn                              │
│r7hnq/ticker tick 2777 from web-8567f99496-r7hnq                              │
└──────────────────────────────────────────────────────────────────────────────┘
 esc back  f follow  / filter  t time  w wrap  p previous  ^s save       ? help
```

`f` pauses and follows again, `/` keeps the matching lines (highlighted;
`tab` for regex or fuzzy), `t` timestamps, `w` wrap, `p` the previous
containers' logs, `c` copies, `ctrl-s` saves to a file (in
`$TMPDIR/litek8s/`), `x` clears. The buffer keeps the last `logs.buffer`
lines (10 000); each stream starts with its last `logs.tail` lines (100).

### Shells and port-forwards

`s` suspends the UI and opens a shell (bash if the image has it, else ash
or sh) in the container, over the API server's exec WebSocket, resized
with the terminal; exiting it comes back to the same view with its exit
code. `F` asks for `local:remote` with the object's ports listed (names
work: `8080:http`); forwards listen on 127.0.0.1 until stopped or the UI
exits, show as `⇄n` in the header, and `f` lists them with their
connection counts.

### xray

```
┌ xray deployments(shop) ──────────────────────────────────────────────────────┐
│▾ deploy api  0/2 ready                                                       │
│  ▾ rs api-6df8b694fd  0/2 ready, revision 1                                  │
│    ▾ pod api-6df8b694fd-ck8sd  CrashLoopBackOff 0/1, 8 restarts              │
│        container busybox  waiting (CrashLoopBackOff), not ready, busybox:1.36│
│    ▾ pod api-6df8b694fd-x697f  CrashLoopBackOff 0/1, 8 restarts              │
│        container busybox  waiting (CrashLoopBackOff), not ready, busybox:1.36│
│▾ deploy cache  1/1 ready                                                     │
│  ▾ rs cache-599d9956dc  1/1 ready, revision 1                                │
│    ▾ pod cache-599d9956dc-r8xlm  Running 1/1                                 │
│        container redis  running, ready, redis:7-alpine                       │
└──────────────────────────────────────────────────────────────────────────────┘
```

`enter` folds a branch; `d`, `y`, `e`, `l`, `s`, `ctrl-d` act on the row's
object (on a container row: its logs and shell); `ctrl-r` reloads it.

### `ui.yaml`

`~/.litek8s/ui.yaml` (next to `config.yaml`; `$LITEK8S_UI_CONFIG` to put it
elsewhere), every part optional:

```yaml
aliases:                        # :cp → pods -n kube-system -l tier=control-plane
  cp: pods -n kube-system -l tier=control-plane
hotkeys:                        # a key runs a command line
  shift-1: ":pods -A"
  f2: "!get events -A --sort-by=.lastTimestamp"
plugins:                        # your commands, on the selected row
  - name: dive
    key: ctrl-i
    scopes: [pods]              # resource names, or all
    command: dive
    args: [$COL-IMAGE]          # $NAME $NAMESPACE $CONTEXT $RESOURCE $CONTAINER $COL-<COLUMN>
  - name: stern
    key: shift-l
    scopes: [deploy]
    command: stern
    args: [-n, $NAMESPACE, $NAME]
    background: true            # output in a pager instead of the terminal
    confirm: false              # true: ask first
skin:                           # names, 0-255 or #rrggbb; default: the terminal's colors
  accent: cyan
  selected: darkgray
  warn: yellow
  bad: red
logs: {tail: 100, buffer: 10000, timestamps: false, wrap: false, maxStreams: 30}
readOnly: false                 # true: no deletes, edits, scaling, shells
kubectl: kubectl                # which kubectl `:kubectl` runs
```

Hotkeys and plugins are checked before the built-in keys, so they can take
any of them. Variables are also passed to plugins as environment variables.
`NO_COLOR` turns colors off (the selection is reverse video then). `--mouse`
selects rows with a click and scrolls with the wheel.

### Kubernetes keys

| where | keys |
|---|---|
| tables | `:` command, `/` filter, `!` kubectl, `?` help, `m` actions, `enter` open, `esc` back, `n` namespace, `0` all namespaces, `space` mark, `<` `>` `O` `N` `A` `S` sort, `ctrl-w` wide, `ctrl-z` faults, `ctrl-r` reload, `f` port-forwards, and [the actions](#acting-on-objects) |
| pagers (describe, YAML, kubectl, help) | `/` search, `n`/`N` next/previous match, `w` wrap, `←→` scroll sideways, `e` edit, `y`/`d` YAML/describe, `m` managedFields, `c` copy, `ctrl-s` save, `esc` back |
| logs | `f` follow/pause, `/` filter, `t` timestamps, `w` wrap, `p` previous, `c` copy, `x` clear, `ctrl-s` save, `esc` back |
| xray | `enter` fold, `d` `y` `e` `l` `s` `ctrl-d` on the row, `ctrl-r` reload |

`1`-`8` and `tab` still switch views, `q` quits.

### For other views and tools

The Kubernetes view's pieces are separate so other parts of litek8s can use
them: `kube::ops` (one async function per API action), `kube::informer`
(compact, watched lists), `kube::describe`, and extra table columns:
a `KMsg::Ext { kind, column, values }` message adds a column (values by
`namespace/name`) to every table of that kind. The CPU and MEM columns
from `metrics.k8s.io` are one; an observability view can add its own
(request rates, error counts) the same way, without touching the table
code.

### Kubernetes performance

Lists are kept as compact rows (the server's cells, name, labels, owners;
no spec, status or annotations), read straight from the API's pages into
them; a table builds only the rows on screen; filtering and sorting run
over the rows in memory on each keystroke. Measured on an M-series MacBook
against a litek8s API server with no nodes (`litek8s apiserver --no-node`)
holding 50, 1 000 and 5 000 pod objects (two containers each, 50
namespaces), both UIs on the pods of all namespaces in a 120x40 `tmux`
window, release build, k9s v0.50.16 (Homebrew), 60 s idle, RSS and CPU
time from `ps` (perf script: create the pods with the API, start the UI,
wait for the list, sample, filter, quit):

| pods | | startup to the list | RSS | CPU idle | filter keystroke to screen |
|---|---|---|---|---|---|
| 50 | **litek8s ui** | 0.09 s | 19.7 MiB | 0.00 % | 0.5 ms (25 ms on screen) |
| | k9s | 1.87 s | 101.7 MiB | 0.40 % | (24 ms on screen) |
| 1 000 | **litek8s ui** | 0.16 s | 29.4 MiB | 0.02 % | 0.6 ms (22 ms on screen) |
| | k9s | 1.64 s | 153.9 MiB | 3.17 % | (23 ms on screen) |
| 5 000 | **litek8s ui** | 0.83 s | 61.0 MiB | 0.02 % | 1.0 ms (30 ms on screen) |
| | k9s | 3.05 s | 359.7 MiB | 15.4 % | (26 ms on screen) |
| 5 000, 10 updates/s | **litek8s ui** | 1.23 s | 43.6 MiB | 1.28 % | 0.9 ms (26 ms on screen) |
| | k9s | – | 361.3 MiB | 15.2 % | (38 ms on screen) |

- *Startup* is from starting the process to the full list on screen,
  watched from outside every 10 ms (it includes `tmux` starting it);
  `LITEK8S_UI_STATS=1` puts litek8s's own figure at 45 ms, 112 ms and
  788 ms (most of it the API server paging 5 000 pods).
- *Filter keystroke*: the first figure is litek8s's own, from the key to
  the frame written (`LITEK8S_UI_STATS`); in brackets, from `tmux
  send-keys` to a changed `tmux capture-pane`, which cannot go below about
  22 ms: both UIs are at that floor.
- *Idle*: pods bound to a node that does not exist, so nothing changes.
  The last two rows are pods the server's scheduler keeps retrying (about
  10 pod updates a second at 5 000 pods): litek8s sends a table at most
  once a second while changes keep coming and redraws only then; k9s
  re-renders its tables every 2 s whether or not anything changed.
- Over SSH on omarchy (Linux x86_64, rootless dev cluster, pods table of a
  namespace with two running pods, 100x30): 0 CPU ticks in 60 s, 24 MiB
  RSS, 2 threads.

## How it talks to litek8s

Only through what litek8s already serves, so a remote litek8s looks the
same:

| what | from |
|---|---|
| containers, images, volumes, sizes, logs, exec, prune | the Docker Engine API on the socket (`GET /containers/json`, `/system/df`, `/events`, `POST /exec/{id}/start` upgraded to a raw stream, ...) |
| resource types | discovery: aggregated (`/api`, `/apis` as `APIGroupDiscoveryList`, two requests), else the legacy per-group documents |
| every list | `?limit=2000&includeObject=Metadata` as a server-side Table (`as=Table;g=meta.k8s.io`), then a watch from its resourceVersion (Tables too) |
| describe, YAML, edit, delete, scale, rollouts, cordon, drain | `GET`, `PUT` (with `?dryRun=All` first for edits), `DELETE` with `propagationPolicy`, `PATCH .../scale`, merge and JSON patches, `POST .../eviction` |
| logs | `.../log?follow=true&timestamps=true&tailLines=N` per pod and container |
| shell | `GET .../exec` upgraded to a WebSocket (`v5.channel.k8s.io`, else `v4.channel.k8s.io`) |
| port-forward | `POST .../pods/{name}/portforward` upgraded to SPDY/3.1 (`portforward.k8s.io`), one session per local connection, listening on 127.0.0.1 |
| CPU and memory columns | `metrics.k8s.io` (when the cluster serves it), every 15 s while pods or nodes are shown |
| `:kubectl`, plugins | `kubectl --context <the UI's context>`, the plugin's command, as child processes |
| state, CPU, memory, disk of the dev cluster | the dev supervisor's state file (`dev.json`: mode, port, pid, cgroup or VM process, what it started with), its cgroup or the VM process |
| up, down, delete, new cluster | `litek8s dev up/down/delete --name <cluster>`, run as a child process (its output shows on Home and Clusters) |
| the local clusters, their memory | the clusters' files in the dev data directory (`cluster.json`, `dev.json`), one request to the VM or sandbox (`env.sock`) for every cluster's memory, `GET /api/v1/nodes` of each running one for its Ready nodes |

```bash
litek8s ui --docker-host tcp://10.0.0.5:2375 --context remote   # another litek8s
litek8s ui --docker-host unix:///run/user/1000/litek8s.sock     # e.g. a socket forwarded with ssh -L
litek8s ui -n demo                                              # start in a namespace
```

Without flags the UI follows this machine's dev cluster: the Kubernetes side
uses the API port the running cluster recorded (and reconnects when it
moves), and says "not running" rather than falling back to whatever listens
on 6443.

## Light on the machine

The UI draws only when a key, a window resize or new data changes what is
on screen, and waits on sockets otherwise:

- **No polling for lists.** Containers, images and volumes are re-listed
  only after Docker events (`GET /events`), coalesced over 150 ms; sizes
  (`/system/df`, which walks directories) only after events that change
  them and at most every 2 s. Pods, deployments and namespaces come from
  Kubernetes watches, coalesced over 100 ms.
- **Clusters** lists the clusters every 2 s while the view is on screen
  (their files, one memory request to the VM or sandbox, a node list from
  each running cluster) and not at all otherwise: a cluster nobody looks
  at is not polled.
- **Home's breakdowns** come with its samples: memory and CPU counters
  every 2 s while Home is on screen (on the Mac one request over the VM's
  control channel), the disk categories every 30 s from a walk the API
  server repeats at most once a minute; nothing while Home is hidden.
  Measured on omarchy, Home with the CPU box over a busy pod: 0.08 % CPU,
  1.8 wake-ups/s, 18 MiB RSS.
- **Home** samples the shown cluster's state, CPU and memory every 2 s while Home
  or Free space is on screen, every 10 s on Settings (for its restart
  note), every 60 s otherwise (to notice the API port moving); the data
  directory's size (a directory walk) at most every 30 s and only while it
  is shown. When the engine is down it is retried with backoff up to 10 s.
- **Input** comes from crossterm's event stream, which waits in the
  terminal reader (no timer); while a shell or an editor has the terminal
  the stream is dropped, so nothing else reads keystrokes. The only timers
  are a status message fading after 8 s and, on the views that show ages,
  a redraw every 30 s.
- **Two threads**: `litek8s ui` runs on a single-threaded async runtime
  (the servers in the same binary use one thread per core) plus
  crossterm's terminal reader; a blocking-pool thread comes and goes for
  the data directory walk. The log view keeps the last 5000 lines, each
  cut at 4096 characters.

Measured on Linux x86_64 (omarchy, release build, rootless dev cluster with
a 3-container compose project and a 3-replica deployment, 80x24 in `tmux`,
after two window resizes), idle 60 s in each view; CPU from
`/proc/<pid>/stat`, wake-ups are voluntary context switches of all its
threads:

| view | CPU | wake-ups | RSS | threads |
|---|---|---|---|---|
| Home (samples every 2 s) | 0.07 % | 3.6/s | 16.5 MiB | 3 (the size walk's blocking thread) |
| Clusters (2 clusters, one with 3 nodes; every 2 s) | 0.08 % | 2.3/s | 17.3 MiB | 2 |
| Containers | 0.00 % | 0.3/s | 16.5 MiB | 2 |
| Kubernetes | 0.02 % | 0.4/s | 16.5 MiB | 2 |
| Settings | 0.02 % | 1.0/s | 16.6 MiB | 2 |
| a log following 1 line/s | 0.03 % | 2.4/s | 19.1 MiB (peak) | 2 |

(The Clusters row is from a later round with named clusters, same machine and
method; in that round Home measured 0.03 % and 1.8/s, Containers 0.00 % and
0.1/s: with Clusters off screen, nothing samples the clusters.)

On macOS (M-series MacBook, `litek8s dev up --cpus 4 --memory 4096`, the
same workload, 80x24 in `tmux`), idle 60 s in each view; CPU from `ps`
CPU time, idle wake-ups and footprint (what Activity Monitor calls Memory)
from `top`:

| view | CPU | idle wake-ups | footprint | RSS | threads |
|---|---|---|---|---|---|
| Home (samples every 2 s) | 0.12 % | 0.02/s | 6.5 MiB | 20.2 MiB | 3 |
| Containers | 0.00 % | 0.02/s | 6.5 MiB | 20.2 MiB | 2 |
| Kubernetes | 0.02 % | 0.02/s | 6.5 MiB | 20.2 MiB | 2 |
| Settings | 0.02 % | 0.02/s | 6.5 MiB | 20.2 MiB | 2 |
| a log following 1 line/s | 0.07 % | 0.03/s | 6.6 MiB | 20.2 MiB | 2 |

(RSS counts the shared pages of the 39 MB binary; the footprint is the UI's
own memory.)

Startup: on Linux the first frame is drawn 0.7 ms after the UI starts and
the first one with the engine's containers 2.7 ms after; on the Mac 1.6-2.8
ms and 7-13 ms (the Docker socket is relayed into the VM over vsock).
`LITEK8S_UI_STATS=1 litek8s ui` prints these, and the number of frames
drawn, when it exits: 156 frames in 315 s for the Linux run above, nearly
all of them Home samples and log lines.

Size: the UI (ratatui and crossterm, no other new dependency) adds to the
release binary:

| | main (c47079e) | with the UI | delta |
|---|---|---|---|
| macOS arm64, stripped | 30.86 MB | 32.43 MB | +1.49 MiB |
| macOS arm64, as built | 37.39 MB | 39.28 MB | +1.80 MiB |
| Linux x86_64, stripped | 35.95 MB | 37.63 MB | +1.61 MiB |
| Linux x86_64, as built (with symbols) | 45.49 MB | 47.65 MB | +2.06 MiB |

Most of it is the UI's own code (about 650 KiB of symbols); ratatui and
crossterm are about 200 KiB.

The Kubernetes view's cluster UI (src/ui/kube) adds 1.92 MiB more to the
stripped macOS arm64 binary (33.41 MB on main c620693, 35.43 MB with it;
40.37 → 42.76 MB as built), with no new dependency.

## Keys

| where | keys |
|---|---|
| everywhere | `1`-`9`/`tab`/`shift-tab` views, `↑↓`/`jk`/`PgUp`/`PgDn`/`g`/`G` move, `/` filter the list (`esc` clears), `ctrl-r` refresh, `?` help, `q` or `ctrl-c` quit |
| Home | `u` up, `d` down (asks), `R` delete the cluster (asks), `b` the next breakdown (memory, CPU, disk) |
| Clusters | `n` new (`tab` between name and nodes), `u` up, `d` down (asks), `D`/`del` delete (asks), `enter` show it (a local cluster: every view, then asks about `kubectl`/`docker`; another context: the Kubernetes view) |
| Containers | `l`/`enter` logs, `s` shell, `u` start, `x` stop (asks), `r` restart (asks), `d`/`del` remove (asks), `space` fold a project |
| Images, Volumes | `d`/`del` remove (asks) |
| Free space | `space` select a category, `p`/`enter` free the selected (asks) |
| Kubernetes | see [its keys](#kubernetes-keys); `?` there lists the ones of what is shown |
| Settings | `enter` edit, `del` back to the default, `space` toggle / switch a context |
| Observe | `l`/`enter` the pod's logs, `L` search every pod's logs, `s` sort by CPU/memory, `n`/`N` namespace, `a` acknowledge the pod's alerts, `A` all (asks) |
| logs | `f` follow, `w` wrap, `c` clear, `esc` back |

`--mouse` turns on clicks (tabs, rows) and the wheel; it is off by default
because it takes over the terminal's own text selection.

## Limits

- Describe is kubectl's for pods, deployments, nodes and services, and its
  generic form (fields as "Title Case" keys) for every other type.
- Rollout undo and history are for deployments (StatefulSets' and
  DaemonSets' ControllerRevisions are not read).
- With `publish: all` the ports listen on every interface, but `docker ps`
  still shows them as `127.0.0.1:<port>`: the engine in the VM or sandbox
  does not know where the host side listens.
- Exec credential plugins that return client certificates (rather than a
  token) are not supported; `auth-provider` only with a token in the
  kubeconfig (OIDC's `id-token`).
