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
| **7 Kubernetes** | namespaces → pods and deployments with status; logs, port-forward, scale, restart, delete pod. For more, [k9s](https://k9scli.io) works with litek8s (`k9s --context litek8s-dev`) |
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
`--memory` (`dev up --memory`, Settings), a ceiling macOS grows the VM to as
the guest uses memory, not memory set aside; on Linux it is the sandbox's
limit when one is set, else the machine's RAM. The memory breakdown repeats
it as its last line ("VM limit (--memory) … a ceiling, not reserved").

Under it, Home explains its figures: what the Resources box's memory, CPU
and disk are made of, one box at a time on 80x24 (`b` shows the next; a
tall terminal shows all three). Memory, on the Mac (a fresh VM, `dev` and a
second cluster idle):

```
┌ Memory · 388 MB (what macOS charges the VM) ─────────────────────────────────┐
│litek8s (dev)            2.7 MB   API server, controllers, node agent         │
│pods & containers           0 B                                               │
│other clusters (1)       1.8 MB   litek8s and pods of the rest                │
│Linux kernel + base      318 MB   fixed cost: kernel, root fs in RAM          │
│cache                   11.1 MB   reclaimable                                 │
│freed in the VM         54.2 MB   macOS takes it back when it needs it        │
└──────────────────────────────────────────────────────────────────────────────┘
```

| line | what it is | from |
|---|---|---|
| litek8s (cluster) | the shown cluster's own processes (API server, controllers, node agent, crun shims) on all its nodes | its cells' cgroups: usage less file pages, less the pods' |
| pods & containers | its pods, Docker containers and image builds | the `pod*`, `docker/*`, `build/*` cgroups under each node |
| other clusters | the same two for every other running cluster, together | their cells |
| Linux kernel + base (macOS) | what the VM costs before anything runs: memory the kernel sets aside at boot (its image and the page structures, ~4 % of the VM's size), its unreclaimable slab, stacks and page tables, and the root filesystem held in RAM (the initramfs: litek8s's binary, crun, iptables); also the VM's own init, cell manager and DHCP client | the VM's size less `MemTotal`, `/proc/meminfo` |
| sandbox (Linux) | the supervisor, pasta and the cell manager | the sandbox's cgroup less the clusters' |
| cache | file pages: images, logs, the binary; the kernel drops them under pressure | `/proc/meminfo` (VM) or the sandbox's `file` pages |
| freed in the VM (macOS) | pages the guest used once and has freed; macOS still counts them for the VM until it needs them back, so a VM that once filled its memory keeps a large figure while idle | the footprint less what the guest uses |

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
│deleted in the VM       83.2 MB   kept by disk.img (it never shrinks)         │
└──────────────────────────────────────────────────────────────────────────────┘
```

| line | what it is |
|---|---|
| litek8s itself | the `litek8s` binary; on macOS also the VM's kernel and initramfs and the signed copy of the binary the VM runs from |
| images | the unpacked layers of every image and of the build cache: one content-addressed store per node, shared by pods, Docker and builds, each layer once |
| container layers | what pod containers and Docker containers wrote (their overlay upper directories) |
| volumes | Docker volumes (named and anonymous), pods' emptyDirs, local PVs, ConfigMap and Secret files |
| logs | pod and container logs (10 MiB and one rotated file each) |
| builds | builds in progress and `RUN --mount=type=cache` directories |
| objects | Kubernetes objects (the write-ahead log, compacted) and the cluster's keys |
| other nodes | a multi-node cluster's workers (each with its own image store) |
| other | metadata, IP allocations, staging |
| rest of the VM disk (macOS) | the other clusters and the filesystem's own blocks |
| deleted in the VM (macOS) | blocks the guest freed that `disk.img` still holds: it is sparse but never shrinks (no TRIM); deleting the last cluster removes it |

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

Kubernetes (`n` cycles namespaces, `v` switches to deployments):

```
 litek8s  1 Home 2 Clust 3 Ctrs 4 Imgs 5 Vols 6 Space 7 Kube 8 Settings 9 Obs
 namespace all   [Pods] Deployments
┌ Pods — litek8s-dev ──────────────────────────────────────────────────────────┐
│NAME                                  READY  STATUS             RESTARTS AGE  │
│default/hello-649565b5b-764k5         1/1    Running            0        27m  │
│default/hello-649565b5b-klzzp         1/1    Running            0        27m  │
│default/hello-649565b5b-kw2tg         1/1    Running            0        27m  │
│demo/echo-55ff5f8478-ps8xb            1/1    Running            0        27m  │
└──────────────────────────────────────────────────────────────────────────────┘
 l logs  f forward  d delete  n namespace  v deployments                 ? help
```

A running port-forward shows in the header (marked "pod gone" if a rollout
replaced its pod):

```
 namespace all   [Pods] Deployments    ⇄ 8080→hello-f8f657f94-qz2gp:80
```

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

## How it talks to litek8s

Only through what litek8s already serves, so a remote litek8s looks the
same:

| what | from |
|---|---|
| containers, images, volumes, sizes, logs, exec, prune | the Docker Engine API on the socket (`GET /containers/json`, `/system/df`, `/events`, `POST /exec/{id}/start` upgraded to a raw stream, ...) |
| pods, deployments, namespaces, logs, scale, restart, delete | the Kubernetes API (list + watch, `PATCH`, `.../log?follow=true`) |
| port-forward | `POST .../pods/{name}/portforward` upgraded to SPDY/3.1 (`portforward.k8s.io`), one session per local connection, listening on 127.0.0.1 |
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

## Keys

| where | keys |
|---|---|
| everywhere | `1`-`9`/`tab`/`shift-tab` views, `↑↓`/`jk`/`PgUp`/`PgDn`/`g`/`G` move, `/` filter the list (`esc` clears), `ctrl-r` refresh, `?` help, `q` or `ctrl-c` quit |
| Home | `u` up, `d` down (asks), `R` delete the cluster (asks), `b` the next breakdown (memory, CPU, disk) |
| Clusters | `n` new (`tab` between name and nodes), `u` up, `d` down (asks), `D`/`del` delete (asks), `enter` show it (a local cluster: every view, then asks about `kubectl`/`docker`; another context: the Kubernetes view) |
| Containers | `l`/`enter` logs, `s` shell, `u` start, `x` stop (asks), `r` restart (asks), `d`/`del` remove (asks), `space` fold a project |
| Images, Volumes | `d`/`del` remove (asks) |
| Free space | `space` select a category, `p`/`enter` free the selected (asks) |
| Kubernetes | `v`/`←→` pods ↔ deployments, `n`/`N` next/previous namespace, `a` all namespaces, `l` logs, `f` port-forward, `X` stop forwards, `d` delete pod (asks), on deployments `s` scale, `r` restart (asks) |
| Settings | `enter` edit, `del` back to the default, `space` toggle / switch a context |
| Observe | `l`/`enter` the pod's logs, `L` search every pod's logs, `s` sort by CPU/memory, `n`/`N` namespace, `a` acknowledge the pod's alerts, `A` all (asks) |
| logs | `f` follow, `w` wrap, `c` clear, `esc` back |

`--mouse` turns on clicks (tabs, rows) and the wheel; it is off by default
because it takes over the terminal's own text selection.

## Limits

- The Kubernetes view is deliberately small: pods and deployments. k9s
  covers the rest and works against litek8s.
- Exec is into Docker containers; for pods use `kubectl exec -it` (or k9s).
- With `publish: all` the ports listen on every interface, but `docker ps`
  still shows them as `127.0.0.1:<port>`: the engine in the VM or sandbox
  does not know where the host side listens.
- Kubeconfig contexts with `exec` or `auth-provider` credentials (cloud
  clusters) are not supported by the UI's Kubernetes client.
