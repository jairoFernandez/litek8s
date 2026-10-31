---
title: "One binary instead of Docker Desktop, and it is also Kubernetes"
description: "litek8s dev up serves the Docker Engine API: docker, compose and Testcontainers work unchanged, and kubectl and Helm use the same cluster. What is missing, too."
date: 2026-10-30
tags: [docker, compose, testcontainers, local development, macos, linux]
docs: [docker, dev, quickstart, numbers, platforms, limitations]
translation: es/un-binario-en-vez-de-docker-desktop
---

A typical laptop setup for backend work has two engines. One runs
containers: Docker Desktop, OrbStack, Colima, or dockerd on Linux. The
other runs Kubernetes: kind or k3d inside the first one, or the Kubernetes
that Docker Desktop can switch on. Two things to install, two to keep
updated, two places your images live.

`litek8s dev up` is one command that gives you both. It starts a local
Kubernetes cluster and, on a socket next to it, the Docker Engine API. The
real `docker` CLI, `docker compose`, Dev Containers and Testcontainers talk
to that socket unchanged; `kubectl` and Helm talk to the same cluster.

## Two minutes

```bash
litek8s dev up
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock

docker run --rm -it alpine sh
docker compose up -d --build

kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')
```

The part we use most: the Docker socket and the cluster share one image
store, so an image you build is a pod's `image:` right away. No registry,
no `kind load`:

```bash
DOCKER_BUILDKIT=0 docker build -t myapp:dev .   # litek8s's builder
kubectl create deployment myapp --image=myapp:dev
```

It runs **next to** an installed Docker or OrbStack, on its own socket.
Nothing of theirs is stopped, changed or used. Leave `DOCKER_HOST` unset
and you talk to them as before, or switch with a context:

```bash
docker context create litek8s --docker host=unix://$HOME/.litek8s/docker.sock
docker context use litek8s        # docker context use default (or orbstack) to go back
```

So trying it costs nothing: you do not have to uninstall anything first.

## Where the containers run

- **macOS (Apple silicon):** in a small Linux VM on Apple's
  Virtualization.framework, started by litek8s itself. No Docker Desktop,
  no Lima, no sudo. `$HOME`, `/private/tmp` and `/private/var/folders` are
  shared at the **same paths**, so `-v $PWD:/app` and Testcontainers' temp
  files work, and published ports listen on `127.0.0.1` and `::1`. With
  Rosetta installed, `--platform linux/amd64` images run.
- **Linux (x86_64, aarch64):** in a rootless sandbox of its own namespaces,
  on crun, with no daemon, no `docker` group and no setuid binary. It does
  not touch the host's network interfaces, iptables rules or an installed
  Docker.

## The numbers

Container operations, measured with `bench/docker-bench.sh` (the same
protocol against every engine, images pulled beforehand). Linux: an Ubuntu
24.04 arm64 guest (4 vCPU, 6 GiB), litek8s rootless against Docker 29.1 in
the same guest. macOS: an M3 Max, the litek8s VM against OrbStack.

| | litek8s (Linux) | dockerd (Linux) | litek8s (macOS VM) | OrbStack |
|---|---|---|---|---|
| `docker run --rm alpine true` | **54 ms** | 179 ms | **89 ms** | 295 ms |
| nginx `run -d -p` until it answers | **52 ms** | 135 ms | **91 ms** | 175 ms |
| compose up (postgres healthy, client ran) | **1.7 s** | 2.0 s | **2.8 s** | 3.1 s |
| build, no cache (python app) | **0.05 s** | 0.37 s | 0.74 s | **0.61 s** |
| build, all cached | **0.02 s** | 0.15 s | 0.48 s | **0.28 s** |
| memory, idle | **35 MiB** | 113 MiB | | |
| memory, postgres + nginx running | **64 MiB** | 149 MiB | | |

Builds are litek8s's own builder (`DOCKER_BUILDKIT=0`). Notice that on
macOS OrbStack builds faster. The Linux memory column is litek8s's whole
sandbox, Kubernetes API server, controllers and node agent included,
against dockerd + containerd + the containers. OrbStack's memory cannot be
separated from its other machines, so it is not compared, and Docker
Desktop was not benchmarked at all.

And the Kubernetes side, from `litek8s dev up` to a Ready node, against
kind and k3d ([Numbers](/docs/numbers/)):

| | litek8s | kind | k3d |
|---|---|---|---|
| Linux (Ubuntu 24.04 arm64 guest) | **0.17 s** | 31.6 s | 11.1 s |
| macOS (M3 Max; kind and k3d on OrbStack) | **1.28 s** | 34.3 s | 10.4 s |
| Idle memory, Linux | **47 MiB** | 526 MiB | 398 MiB |

## What already works

`compat/docker/run.sh` runs 17 cases that pass on Linux x86_64 rootless and
in the macOS VM. Roughly: `docker run` with the usual flags (ports,
volumes, env, healthchecks, `--restart`, limits, `--privileged`), `exec`,
`logs -f`, `cp`, `stats`, user-defined networks with aliases,
`host.docker.internal`, named volumes, compose with healthchecks and
`depends_on: condition: service_healthy`, the `devcontainer` CLI that VS
Code and Cursor run, and Testcontainers with random ports, wait strategies
and its Ryuk reaper.

Real projects ran unchanged, in copies: a FastAPI + Postgres backend on
macOS with uvicorn hot reload of Mac edits (an edit reaches a watcher in
the container in 30-70 ms), and on Linux x86_64 a Prometheus + Grafana
stack, MySQL + Redis with `restart: always`, and a Rails app with MariaDB
built from its Dockerfile (5 min 10 s with litek8s's builder, 6 min 28 s
through buildx's BuildKit container).

One thing that works differently on purpose: Docker and OrbStack limit
containers one by one, while litek8s puts every container of a compose
project in one cgroup, so you can give the whole stack a budget:

```yaml
services:
  api:
    labels:
      dev.litek8s.project.memory: 2Gi
      dev.litek8s.project.cpus: "2"
```

When the stack hits it, the OOM killer acts inside the project, on the
runaway service, and the rest of your machine never notices.

## What is missing or different

This is the section to read before you uninstall anything.

- **Not supported (yet):** `docker push`, `commit`, `export`/`import`,
  `diff`, swarm and services, plugins and other volume or network drivers,
  IPv6 on container networks, `--init`, `--device`, GPU, published UDP
  ports, the BuildKit API itself (buildx's `docker` driver), and
  `docker update` (accepted, changes nothing).
- **buildx needs `--load`.** If buildx is installed (Docker Desktop,
  OrbStack and most Linux packages ship it), plain `docker build` goes
  through a BuildKit container and the image does not show up in
  `docker images`, nor for pods, without `--load`. Use `DOCKER_BUILDKIT=0`
  or a builder with `default-load=true`. `docker compose up --build` loads
  either way.
- **Containers are not pods.** `kubectl get pods` does not list Docker
  containers. They share the image store and DNS with the cluster, not the
  object model.
- **Containers do not survive `litek8s dev down`**; restart policies bring
  them back on `dev up`.
- **Ports:** published on `127.0.0.1` only (and `::1` on macOS), never on
  other interfaces; rootless Linux cannot bind host ports below 1024.
- **Two engines, one shell:** with Docker or OrbStack also running, check
  which one you are talking to (`docker version --format
  '{{.Server.Platform.Name}}'` says `litek8s` here). A port the other
  engine already publishes fails with `port is already allocated`.
- **macOS memory:** Virtualization.framework never gives a VM's memory back
  to macOS once the guest has touched it. litek8s keeps the guest small
  and grows it on demand, but after a big build the way to get memory back
  is `litek8s dev down && litek8s dev up`.
- **A young project:** litek8s is 0.x; flags and on-disk formats may change
  between releases ([Limitations](/docs/limitations/)).

## Platforms

| | dev clusters and the Docker API |
|---|---|
| macOS, Apple silicon (13+) | yes, in a VM (Virtualization.framework) |
| Linux x86_64 / aarch64 | yes, rootless (kernel ≥ 5.11, cgroup v2) |
| macOS Intel, Windows | no (WSL2 untested) |

On Linux it needs `crun`, `uidmap`, `passt` (or `slirp4netns`), `iproute2`
and `iptables`; `litek8s dev setup` checks and says what is missing, and
Ubuntu ≥ 23.10 needs `sudo "$(command -v litek8s)" dev setup` once for
AppArmor. macOS needs nothing else.

## Try it

```bash
brew install jairoFernandez/tap/litek8s
```

or

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Then `litek8s dev up`, point `DOCKER_HOST` at the socket, and run the
compose file you already have. Every flag and what each command supports
is in [Docker API](/docs/docker/); the cluster side in
[Dev clusters](/docs/dev/).
