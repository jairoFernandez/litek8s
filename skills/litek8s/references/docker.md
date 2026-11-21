# The Docker API of a dev cluster

`litek8s dev up` also serves the Docker Engine API, so the real `docker`
CLI, `docker compose`, Dev Containers and Testcontainers run on litek8s,
next to (not instead of) an installed Docker or OrbStack.

```bash
DOCKER_HOST=unix://$HOME/.litek8s/docker.sock docker run --rm alpine echo hi     # cluster dev
DOCKER_HOST=unix://$HOME/.litek8s/N/docker.sock docker compose up -d --build     # cluster N
```

MCP tools (read-only): `docker_ps`, `docker_images`, `docker_logs` with
`cluster: N`. They refuse a socket whose engine is not litek8s's.
For anything else, run the docker CLI with `DOCKER_HOST` set as above.

## Name the socket in every command

Put `DOCKER_HOST=...` (or `docker --host unix://...`) **on each docker
command**, never only in an earlier `export`. An agent's shell tool
usually starts a new shell per call: an `export` from one call is gone in
the next, and the command silently talks to the machine's own Docker
(`/var/run/docker.sock`, OrbStack, Docker Desktop) instead. With both
engines up the results mix and look like a broken engine: a container
"stuck in `created`" on litek8s while "its" port answers (it is the other
engine's container on that port), a `docker start` or `inspect` that sees
another set of containers, `port is already allocated` for a port nothing
on litek8s holds. Do not run `docker context use` either unless the user
asks: it changes their default for every terminal.

Check which engine answers before trusting a result:

```bash
DOCKER_HOST=unix://$HOME/.litek8s/N/docker.sock docker version --format '{{.Server.Platform.Name}}'   # litek8s
```

litek8s says `litek8s` (engine version 27.5.1); anything else is another
engine. Before blaming litek8s for a hang or wrong state, repeat the
failing step with the socket named on that command, on a fresh cluster
(`cluster_up name=<task>`), one variable at a time.

Ports: litek8s publishes on the host's `127.0.0.1`, so a port the
machine's Docker already publishes (`docker ps` with `DOCKER_HOST` unset)
fails with `port is already allocated`. Stop that container or map
another host port; do not stop the user's containers without asking.

## Containers are not pods

Docker containers here are runtime-level containers: `kubectl get pods`
does not show them and `docker ps` does not show pods. They share the
cluster's **image store**: an image built on the cluster's socket is
`image: myapp:dev` for a pod right away (no push, no `dev load`) — as
long as the build lands in the store and not in a BuildKit container
([Builds](#builds)).

## Works

`run` (ports, volumes, env, networks, restart policies, healthchecks,
`--platform`), `ps/logs/exec/stop/start/rm/inspect/cp/top/stats/wait`,
images (`pull/push/images/tag/rmi/save/load`), `build` (classic builder
plus BuildKit Dockerfile syntax: `RUN --mount=type=cache`, heredocs, ...),
networks (bridges with DNS names and aliases), volumes, compose
(`up/down/ps/logs/exec/build`, healthchecks, `depends_on`), Dev Containers,
Testcontainers (Ryuk included). On Apple silicon, `--platform linux/amd64`
runs under Rosetta when installed.

Published ports listen on `127.0.0.1` (and `::1` on macOS) only.

## Builds

`docker build` works, but which builder runs it depends on the `docker`
CLI, and only one of them leaves the image in the cluster by itself:

- `DOCKER_BUILDKIT=0 docker build -t myapp:dev .` uses **litek8s's own
  builder**: the image lands in the image store, `FROM` can use an image
  that only exists in litek8s, and no extra container runs. Works with
  any `docker` CLI.
- **With the buildx plugin** (Docker Desktop, OrbStack and most Linux
  packages ship it), plain `docker build` goes through buildx, whose
  default builder here is a `moby/buildkit` container it starts on
  litek8s. Full BuildKit (secrets, SSH mounts, cache export,
  multi-platform), but the image **stays in that builder unless you pass
  `--load`**, so pods do not see it; and it cannot `FROM` an image that
  only exists in litek8s (it pulls from registries).
- `docker compose up --build` and `compose build` load the result into
  litek8s either way. `DOCKER_BUILDKIT=0 COMPOSE_BAKE=false` picks
  litek8s's builder for compose.

So a tag that is nowhere after a successful build went to a BuildKit
container: rerun with `--load`, or with `DOCKER_BUILDKIT=0`.

## Not supported (yet)

`commit`, `export/import`, `diff`, swarm/services, plugins, other network
or volume drivers, IPv6 networks, `--init`, `--device`, GPUs, published
UDP ports, `checkpoint`, `docker stats` beyond CPU, memory and PIDs, the
BuildKit API itself (`/grpc`, `/session`: buildx's `docker` driver — its
`docker-container` driver works, see [Builds](#builds)). Containers stop
with `litek8s dev down`; restart policies bring them back on `dev up`.
