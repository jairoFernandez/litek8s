# `litek8s ui`: the dev environment in a terminal

`litek8s ui` is litek8s's answer to OrbStack's and Docker Desktop's windows:
see, configure and operate the local dev environment without remembering
commands. It is part of the same binary, keyboard-driven, fits an 80x24
terminal, and works the same locally and over SSH:

```bash
litek8s ui                  # this machine's dev cluster
ssh -t box litek8s ui       # another machine's, in your terminal
```

Seven views, `1`-`7` (or `tab`) to switch, `?` for the keys of each:

| view | what it gives you |
|---|---|
| **1 Home** | the dev cluster's state (up, down, starting, stopping), mode, uptime; CPU, memory and disk of the VM or rootless sandbox; the Docker socket and API server; whether `docker` and `kubectl` point at litek8s. `u` up, `d` down, `R` reset (asks first) |
| **2 Containers** | `docker ps -a`, grouped by compose project: `l` logs (follow), `s` a shell in the container, `u`/`x`/`r` start/stop/restart, `d` remove; on a project row they act on the whole project, `space` folds it |
| **3 Images** | sizes, age, which containers and pods use each; `d` remove |
| **4 Volumes** | sizes, which containers mount each, compose project; `d` remove |
| **5 Free space** | what can be reclaimed: stopped containers, dangling images, build cache, unused images, unused volumes; select with `space`, `p` frees it |
| **6 Kubernetes** | namespaces → pods and deployments with status; logs, port-forward, scale, restart, delete pod. For more, [k9s](https://k9scli.io) works with litek8s (`k9s --context litek8s-dev`) |
| **7 Settings** | CPUs, memory, shared directories, API port, where published ports listen; switch the docker and kubectl contexts to litek8s and back; edit the settings file in `$EDITOR` |

Everything that deletes or stops asks first, and says what goes: removing a
compose project lists its containers, a reset says it deletes every object,
image and volume, freeing space adds up what it frees.

## Screens

Captured from real sessions on the macOS VM and, over SSH, the Linux
rootless sandbox; the test runs used scratch paths and a second API port,
shown here as a default install has them (`~/.litek8s`, `~/.kube/config`,
6443).

Home:

```
 litek8s  1 Home  2 Containers  3 Images  4 Volumes  5 Space  6 Kube  7 Settings
┌ Dev cluster ─────────────────────────┐┌ Resources ───────────────────────────┐
│● up  macOS VM, up 22m 50s            ││CPU             1% of 4 CPUs          │
│API      http://127.0.0.1:6443        ││Memory ████    466 MB / 4.1 GB        │
│Docker   ~/.litek8s/docker.sock       ││Disk      269 MB used, 131 GB free    │
│Engine   API 27.5.1 (aarch64)         ││Ctrs   3/3 running   Pods 4/4         │
│VM IP    192.168.64.21                ││Images 4  (200 MB)                    │
└──────────────────────────────────────┘└──────────────────────────────────────┘
┌ Your tools point at ─────────────────────────────────────────────────────────┐
│✗ docker context   default                                                    │
│  DOCKER_HOST      unset                                                      │
│✓ kubectl context  litek8s-dev  (~/.kube/config)                              │
└──────────────────────────────────────────────────────────────────────────────┘
 Switch docker/kubectl to litek8s in Settings (7). Help: ?

 u up  d down  R reset  7 settings                                       ? help
```

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
┌ Dev cluster ─────────────────────────┐┌ Resources ───────────────────────────┐
│● up  rootless sandbox, up 20s        ││CPU    ████████200% of 2 CPUs ████████│
│API      http://127.0.0.1:6443        ││Memory █      54.8 MB / 2.1 GB        │
```

Containers, with a compose project and a confirmation:

```
 litek8s  1 Home  2 Containers  3 Images  4 Volumes  5 Space  6 Kube  7 Settings
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
 litek8s  1 Home  2 Containers  3 Images  4 Volumes  5 Space  6 Kube  7 Settings
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
│API server port      6443 (default)                                           │
│Published ports on   all interfaces (LAN)                                     │
│docker context       default  (enter: use litek8s)                            │
│kubectl context      other  (enter: use litek8s-dev)                          │
│Config file          ~/.litek8s/config.yaml  (enter: $EDITOR)                 │
└──────────────────────────────────────────────────────────────────────────────┘
 ⟳ Restart needed for published ports, shared directories: Home (1), d then u.
 CPUs, memory and shares size the macOS VM.
 Settings apply on the next `litek8s dev up` (its flags win).
```

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
  context for `unix://~/.litek8s/docker.sock` and makes it current, with
  `docker context` when the CLI is installed and by writing the CLI's
  context store (`$DOCKER_CONFIG/contexts`, `config.json`) when it is not.
  "switch back" returns to the context that was current before
  (`default` if none was recorded). `DOCKER_HOST`, when set in your shell,
  wins over any context; Home says so.
- **kubectl context.** "use litek8s-dev" makes the dev cluster's context
  current (adding it if the cluster runs); "switch back" restores the
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
| up, down, reset | `litek8s dev up/down/reset`, run as a child process (its output shows on Home) |

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

- **No polling for lists.** Containers, images and volumes are re-listed
  only after Docker events (`GET /events`), coalesced over 150 ms; sizes
  (`/system/df`, which walks directories) only after events that change
  them and at most every 2 s. Pods, deployments and namespaces come from
  Kubernetes watches, coalesced over 100 ms.
- **Home** samples the cluster's CPU and memory every 2 s while Home or
  Free space is on screen, every 10 s otherwise; the data directory's size
  (a directory walk) at most every 30 s.
- The screen redraws when something changes (and every 5 s for ages).
- Size: the UI adds about 1.4 MiB to the stripped release binary
  (macOS arm64 30.9 → 32.4 MB, Linux x86_64 36.0 → 37.5 MB): ratatui and
  crossterm, no other new dependency.

## Keys

| where | keys |
|---|---|
| everywhere | `1`-`7`/`tab`/`shift-tab` views, `↑↓`/`jk`/`PgUp`/`PgDn`/`g`/`G` move, `/` filter the list (`esc` clears), `ctrl-r` refresh, `?` help, `q` or `ctrl-c` quit |
| Home | `u` up, `d` down (asks), `R` reset (asks) |
| Containers | `l`/`enter` logs, `s` shell, `u` start, `x` stop (asks), `r` restart (asks), `d`/`del` remove (asks), `space` fold a project |
| Images, Volumes | `d`/`del` remove (asks) |
| Free space | `space` select a category, `p`/`enter` free the selected (asks) |
| Kubernetes | `v`/`←→` pods ↔ deployments, `n`/`N` next/previous namespace, `a` all namespaces, `l` logs, `f` port-forward, `X` stop forwards, `d` delete pod (asks), on deployments `s` scale, `r` restart (asks) |
| Settings | `enter` edit, `del` back to the default, `space` toggle / switch a context |
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
