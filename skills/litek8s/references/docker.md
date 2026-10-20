# The Docker API of a dev cluster

`litek8s dev up` also serves the Docker Engine API, so the real `docker`
CLI, `docker compose`, Dev Containers and Testcontainers run on litek8s,
next to (not instead of) an installed Docker or OrbStack.

```bash
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock     # cluster dev
export DOCKER_HOST=unix://$HOME/.litek8s/N/docker.sock   # cluster N
docker run --rm alpine echo hi
docker compose up -d --build
# or: docker context use litek8s   (litek8s-N for a named cluster)
```

MCP tools (read-only): `docker_ps`, `docker_images`, `docker_logs` with
`cluster: N`. They refuse a socket whose engine is not litek8s's.
For anything else, run the docker CLI with `DOCKER_HOST` set as above.
Do not change the user's global docker context unless asked; prefer
`DOCKER_HOST=... docker ...` per command.

## Containers are not pods

Docker containers here are runtime-level containers: `kubectl get pods`
does not show them and `docker ps` does not show pods. They share the
cluster's **image store**: `docker build -t myapp:dev .` on the cluster's
socket makes `image: myapp:dev` usable by pods right away (no push, no
`dev load`).

## Works

`run` (ports, volumes, env, networks, restart policies, healthchecks,
`--platform`), `ps/logs/exec/stop/start/rm/inspect/cp/top/stats/wait`,
images (`pull/images/tag/rmi/save/load`), `build` (classic builder plus
BuildKit Dockerfile syntax: `RUN --mount=type=cache`, heredocs, ...),
networks (bridges with DNS names and aliases), volumes, compose
(`up/down/ps/logs/exec/build`, healthchecks, `depends_on`), Dev Containers,
Testcontainers (Ryuk included). On Apple silicon, `--platform linux/amd64`
runs under Rosetta when installed.

Published ports listen on `127.0.0.1` (and `::1` on macOS) only.

## Not supported (yet)

`docker push`, `commit`, `export/import`, `diff`, swarm/services, plugins,
other network or volume drivers, IPv6 networks, `--init`, `--device`, GPUs,
published UDP ports, the BuildKit API (buildx's docker driver: use
`DOCKER_BUILDKIT=0`, and `COMPOSE_BAKE=false` for compose). Containers stop
with `litek8s dev down`; restart policies bring them back on `dev up`.
