# The Docker API: litek8s instead of Docker Desktop / OrbStack

`litek8s dev up` also serves the Docker Engine API, so the real `docker` CLI,
`docker compose`, VS Code / Cursor Dev Containers and Testcontainers run
their containers on litek8s:

```bash
litek8s dev up
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
docker run --rm -it alpine sh
docker compose up -d --build
```

It runs **next to** an installed Docker or OrbStack, on its own socket:
nothing of theirs is stopped, changed or used. Leave `DOCKER_HOST` unset to
talk to them as before. To switch with `docker context` instead of an
environment variable (this only adds a CLI context):

```bash
docker context create litek8s --docker host=unix://$HOME/.litek8s/docker.sock
docker context use litek8s        # docker context use default (or orbstack) to go back
```

The socket is `$LITEK8S_DOCKER_SOCKET` if set. `litek8s dev down` stops the
containers with the cluster; `dev up` brings back the ones with a restart
policy (`always`, `unless-stopped`), as Docker does after a reboot.

## Where containers run

- **Linux:** in the rootless dev sandbox (user, network, mount, PID and
  cgroup namespaces of their own, [dev.md](dev.md)), on crun, with no
  daemon. The host's network, iptables rules and Docker are not touched.
- **macOS:** in the dev VM (Virtualization.framework), on crun. The Mac
  side relays the socket over vsock, listens on `127.0.0.1` **and** `::1`
  for every published port, shares `$HOME`, `/private/tmp` and
  `/private/var/folders` at the same paths (so `-v $PWD:/app`,
  `-v /tmp/x:/x` and Testcontainers' temp files work), passes file change
  events into the VM, and shares Rosetta.

### Containers, not pods

A Docker container here is a runtime-level container, not a Kubernetes
Pod. Docker semantics do not fit Pods: a container is created and started
separately, attached to before it runs, stopped and started again **with
its files**, copied into while stopped (Testcontainers does that), lives
on networks of its own with aliases, and keeps its exit code until it is
removed. So the engine (`src/docker/`) keeps its own records and drives
crun through the same per-container shim the pod runtime uses, with the
writable layer kept between runs and an attach socket for live stdin and
stdout. What it shares with the cluster: the image store (an image you
`docker build` can be a pod's `image:` right away, and `kubectl` workloads
reuse pulled layers), the DNS server, the crun shim and bundle code.
`kubectl get pods` does not list Docker containers.

## What works

Tested by `compat/docker/run.sh` (17 cases, all passing on Linux x86_64
rootless and on the macOS VM; run it against Docker or OrbStack too, where
the litek8s-only `compose_budget` case is skipped):

| area | supported |
|---|---|
| `docker run` | `-d`, `--rm`, `-it` (pty, resize, detach keys), `-i` stdin, `-p`/`-P`, `-v`/`--mount` (bind, volume, tmpfs), `-e`/`--env-file`, `-w`, `-u`, `--name`, `--network`, `--network-alias`, `--entrypoint`, `--restart`, `--health-*`, `--add-host` (incl. `host-gateway`), `--platform`, `--cap-add/--cap-drop`, `--privileged`, `--read-only`, `--shm-size`, `--ulimit`, `--sysctl`, `--memory`, `--cpus`, `--pid`/`--network container:` |
| containers | `ps`, `logs` (`-f`, `--tail`, `--since`, `-t`), `exec` (`-it`, `-u`, `-e`, `-w`), `stop`/`start`/`restart`/`kill`/`pause`, `rm`, `inspect`, `rename`, `wait`, `cp` (both ways, also before start), `top`, `stats`, `port`, `prune`, `events` |
| images | `pull` (progress, `--platform`; `docker login` credentials are passed on but untested against a private registry), `images`, `tag`, `rmi`, `inspect`, `history`, `save`, `load`, `prune` |
| build | `docker build` with the classic API: Dockerfile cache, multi-stage, `--target`, `--build-arg`, `.dockerignore`, `-f` outside the context; plus the BuildKit Dockerfile syntax (`RUN --mount=type=cache\|bind\|tmpfs`, `COPY --chmod`/`--link`, heredocs, `TARGETARCH` etc.) |
| networks | `network create/ls/inspect/rm/connect/disconnect/prune`; one bridge per network, names, aliases and short IDs resolve on user-defined networks, networks are isolated from each other; `host.docker.internal` reaches the host |
| volumes | named and anonymous volumes (filled from the image on first use, like Docker), `local` driver bind options (`o=bind,device=`), `volume ls/inspect/rm/prune` |
| compose | `up`/`down`/`ps`/`logs -f`/`exec`/`restart`/`build`/`pull`, project networks with service names and aliases, healthchecks and `depends_on: condition: service_healthy`, env files, named and anonymous volumes, bind mounts, published ports, restart policies, per-service limits (`mem_limit`, `cpus`, `pids_limit`, `deploy.resources.limits`), and a budget for the whole project (next section) |
| Dev Containers | the `devcontainer` CLI (what VS Code and Cursor run): features, `postCreateCommand`, `remoteUser`, `containerEnv`, `mounts`, exec, file ownership |
| Testcontainers | random ports (`-P`) through inspect, log and port wait strategies, exec, copy in/out, the Ryuk reaper (it mounts the Docker socket: any bind source named `docker.sock` is this engine's own socket inside containers) |

## Resource budgets for compose projects

Docker and OrbStack limit containers one by one. A compose stack on a
laptop is better thought of as one machine: litek8s puts every container
of a compose project (label `com.docker.compose.project`) in one cgroup,
`litek8s/docker/projects/<project>`, and that cgroup can have a budget:

- **memory** (`memory.max`, no swap): the whole stack together. When it is
  reached the kernel's OOM killer acts *inside the project*: it picks the
  biggest process there (the runaway service), not something else in the
  VM, and other projects and the cluster never notice. With
  **oom_group** every container of the project is killed instead
  (`memory.oom.group`), like a server that ran out of memory.
- **cpus** (`cpu.max`): the stack shares that many CPUs.
- **pids** (`pids.max`): a fork bomb stays in its project.

Per-container limits (`mem_limit`, `cpus`, ...) still apply inside the
budget. A budget comes from `~/.litek8s/config.yaml`:

```yaml
docker:
  projects:                 # by compose project name
    myapp: {memory: 2Gi, cpus: 2}
    kafka-lab: {memory: 6Gi, cpus: 3, pids: 2000, oom_group: true}
  default_project:          # any project nothing else gives a budget
    memory: 4Gi
  defaults:                 # a container that sets no limit of its own (off when absent)
    memory: 1Gi
    cpus: 1
```

or from a label on any service of the project, so the budget can travel
with the compose file:

```yaml
services:
  api:
    labels:
      dev.litek8s.project.memory: 2Gi
      dev.litek8s.project.cpus: "2"
      dev.litek8s.project.pids: "1000"
      dev.litek8s.project.oom-group: "true"
```

Field by field, the config's entry for the project wins over labels
(your machine, your limits), labels win over `default_project`; when
several services' labels disagree the smallest wins. Sizes are `2Gi`,
`512Mi` or `1g` (binary, as Docker reads them; a bare number is MiB, like
the top-level `memory:`), CPUs `2`, `1.5` or `"500m"`. Labels that do not
parse fail `docker create` with the reason. `defaults` fill in only what
a container leaves unset (memory, cpus, pids); `docker inspect` still
shows what the client asked for.

`litek8s dev up` sends the `docker:` section to the cluster's Docker API,
which keeps it (`<data>/docker/settings.json`) and applies it to running
projects at once; for a cluster that already runs, `litek8s dev up` again
(or saving the file from `litek8s ui`'s Settings) applies an edited file
without restarting anything. Labels apply when a container of the project
starts. `GET /litek8s/v1/docker/projects` on the Docker socket lists each
project's budget and use; `GET`/`PUT /litek8s/v1/docker/settings` read and
replace the settings.

What you see:

- `docker stats` shows the limit that really caps a container: the
  smallest of its own, its project's budget and the cluster's (`MEM USAGE
  / LIMIT` is `… / 256MiB` for every service of a 256Mi project), and PIDs.
- An OOM-killed container exits with **137**, `docker inspect` says
  `"OOMKilled": true`, and Docker's `oom` event carries which limit it
  hit: `litek8s.oom=project` (with `litek8s.project.memory=256Mi`),
  `container` (its own `mem_limit`) or `system`. The engine's log says it
  in words: `docker: container shop-worker-1 (3f2a…) was OOM-killed: out
  of compose project shop's budget (memory 256Mi)`.
- `litek8s ui`'s Containers view puts a memory meter against the budget,
  the CPUs in use and the OOM kills on each project's row, marks killed
  containers `OOMKilled` and says so in the status line
  ([ui.md](ui.md#containers-and-budgets)).

`compat/docker/run.sh compose_budget` runs it end to end: a 256Mi project
whose `tail /dev/zero` service is killed while its other service and
another project keep running, and the same with `oom-group`.

## Builds: two builders

The engine reports the classic builder (`Builder-Version: 1`). What runs
your build then depends on the `docker` CLI:

- **Without the buildx plugin**, or with `DOCKER_BUILDKIT=0`, `docker build`
  uses **litek8s's builder**: every `RUN` is a crun container on an overlay
  of the image's layers whose upper directory becomes the new layer, steps
  are cached by a key over the instructions, build args and the content of
  copied files (a fully cached rebuild gives the same image ID). Fastest
  here; no extra container.
- **With buildx installed** (Docker Desktop, OrbStack and most Linux
  packages ship it), `docker build` and compose's builds go through buildx,
  whose default builder for an engine without BuildKit is a `moby/buildkit`
  container (driver `docker-container`), which buildx starts on litek8s.
  Full BuildKit (secrets, SSH mounts, cache export, multi-platform), and
  compose loads the result into litek8s. Plain `docker build` needs `--load`
  for the image to show up in `docker images`, or a builder that loads by
  default:

  ```bash
  docker buildx create --use --name litek8s --driver docker-container --driver-opt default-load=true
  ```

  (builders are per endpoint: this does not change the builder of your
  Docker or OrbStack context).

  To use litek8s's builder instead: `DOCKER_BUILDKIT=0` for `docker build`,
  `DOCKER_BUILDKIT=0 COMPOSE_BAKE=false` for compose (the CLI prints a
  deprecation notice for the legacy builder; it still works).

A BuildKit container cannot use images that exist only in litek8s (it pulls
`FROM` images from registries). That matters for **Dev Containers on
Linux**: the CLI's step that changes the remote user's UID builds `FROM`
the image it just built. With buildx installed, run the devcontainer CLI
with the classic builder:

```bash
DOCKER_BUILDKIT=0 devcontainer up --buildkit never --workspace-folder .
```

or, since litek8s keeps your UID the same inside (below), set
`"updateRemoteUserUID": false` in `devcontainer.json` when your UID is the
image user's (usually 1000). On macOS the CLI skips that step.

## File ownership in bind mounts

- **Linux:** the rootless sandbox maps your own UID to itself and backs
  every other UID, root included, with your `/etc/subuid` range. A file a
  container creates in a bind mount as UID 1000 (a dev container's
  `vscode` user, `docker run -u $(id -u)`) is yours on the host, as with
  rootful Docker; one created by root belongs to a subordinate id (with
  rootful Docker: to root). `LITEK8S_DEV_IDMAP=root litek8s dev up` maps
  you to root instead (Docker rootless semantics: root's files are yours,
  UID 1000's are not).
- **macOS:** files in shared directories always belong to you, whatever
  UID wrote them.

## Hot reload on macOS

virtiofs shows the VM a file's new content right away but tells the VM's
kernel nothing when it changes on the Mac, so inotify watchers in
containers (uvicorn `--reload`, nodemon, Vite, webpack, Air) would miss
edits. The Mac side watches the directories running containers bind-mount
(FSEvents) and the guest re-applies each changed file's mode, which inotify
reports as `IN_ATTRIB`: an edit reaches a watcher in the container in
30-70 ms. A uvicorn app on a FastAPI + Postgres project reloads with the
change about 3.6 s after the save, nearly all of it the app restarting.

## linux/amd64 on Apple silicon

When Rosetta is installed (`softwareupdate --install-rosetta`), the VM
shares it and registers it for x86-64 binaries:
`docker run --platform linux/amd64 ...` and `platform: linux/amd64` in
compose run amd64 images. As with Docker's classic image store, a tag holds
one platform: pulling `alpine` for amd64 replaces the arm64 `alpine:latest`.

## Ports

Published ports listen on the host's `127.0.0.1` (and `::1` on macOS),
never on other interfaces: `docker ps` shows `127.0.0.1:8080->80/tcp`.
`-P` picks free ports. UDP ports are not published (they stay reachable on
the container's address inside the sandbox/VM). Rootless Linux cannot bind
host ports below 1024.

## Not supported (yet)

`docker push`, `commit`, `export`/`import`, `diff`, `docker swarm` and
services, plugins and other volume or network drivers (`overlay`,
`macvlan`, `ipvlan`), IPv6 on container networks, `--init`, `--device`,
GPU, published UDP ports, the BuildKit API itself (`/grpc`, `/session`:
buildx's `docker` driver), `docker stats` beyond CPU, memory and PIDs, `docker update` (accepted, changes nothing),
`checkpoint`. Containers do not survive `litek8s dev down` (restart
policies bring them back on `dev up`).

## Numbers

`bench/docker-bench.sh` (same protocol against any engine; images pulled
beforehand). Linux: Ubuntu 24.04 arm64 guest (qemu+HVF, 4 vCPU, 6 GiB),
litek8s rootless vs Docker 29.1 (Ubuntu's `docker.io`) in the same guest.
macOS: M3 Max, litek8s dev VM vs OrbStack (OrbStack's own containers and
VMs were running).

| | litek8s (Linux) | dockerd (Linux) | litek8s (macOS VM) | OrbStack |
|---|---|---|---|---|
| `docker run --rm alpine true` | **54 ms** | 179 ms | **89 ms** | 295 ms |
| nginx `run -d -p` until it answers | **52 ms** | 135 ms | **91 ms** | 175 ms |
| compose-basic up (postgres healthy, client ran) | **1.7 s** | 2.0 s | **2.8 s** | 3.1 s |
| build, no cache (python app) | **0.05 s**¹ | 0.37 s | 0.74 s¹ | **0.61 s** |
| build, all cached | **0.02 s**¹ | 0.15 s | 0.48 s¹ | **0.28 s** |
| memory, idle² | **35 MiB** | 113 MiB | | |
| memory, postgres + nginx running² | **64 MiB** | 149 MiB | | |

¹ litek8s's builder (`DOCKER_BUILDKIT=0`). Through buildx's BuildKit
container instead: Linux 0.88 / 0.29 s, macOS 11.5 / 0.76 s (the first
build also starts the BuildKit container; exporting the image to litek8s
dominates).
² cgroup `memory.current - inactive_file`: litek8s's whole sandbox (it
includes the Kubernetes API server, controllers and node agent),
dockerd + containerd + the containers' scopes. OrbStack's memory cannot be
isolated from its other machines and containers, so it is not compared.

Real projects run unchanged (in copies, with other host ports where the
user's Docker held them): a FastAPI + Postgres backend on macOS (build with
BuildKit, Alembic migrations, uvicorn hot reload of Mac edits), and on Linux
x86_64 a Prometheus + Grafana stack (provisioned datasource, scraping
`host.docker.internal`), MySQL + Redis with `restart: always`, and a Rails
app with MariaDB built from its Dockerfile (`bundle install` with native
gems and an asset build: 5 min 10 s with litek8s's builder, 6 min 28 s
through buildx's BuildKit container, exporting included).
