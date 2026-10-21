# Dev clusters (`litek8s dev`)

A local Kubernetes cluster like kind/k3d, without Docker: on macOS all
clusters share one small Virtualization.framework VM, on Linux one rootless
sandbox. Each cluster is a set of namespaces ("cells") inside it, with its
own API port, data, Docker socket and kube context.

## Lifecycle

| | MCP tool | CLI |
|---|---|---|
| list | `clusters_list` | `litek8s dev list -o json` |
| status | `cluster_status name=N` | `litek8s dev status --name N -o json` |
| create/start | `cluster_up name=N nodes=K` | `litek8s dev up --name N --nodes K --no-switch` |
| stop (keeps objects, images) | `cluster_down name=N confirm=true` | `litek8s dev down --name N` |
| stop one worker | `cluster_down name=N node=litek8s-N-worker confirm=true` | `litek8s dev down --name N --node litek8s-N-worker` |
| start it again | `cluster_up name=N node=litek8s-N-worker` | `litek8s dev up --name N --node litek8s-N-worker` |
| delete (objects, images, volumes, context) | `cluster_delete name=N confirm=true` | `litek8s dev delete --name N` |
| load a local image | – | `litek8s dev load --name N myapp:dev` (or a `docker save` .tar) |

- `dev up` waits until every node is Ready (`--timeout`, default 120 s).
  First start of the VM takes a few seconds; later clusters ~1-2 s.
- `--nodes K` (1-10, remembered): control plane `litek8s-N` plus workers
  `litek8s-N-worker`, `-worker2`, ... joined with the real join flow and a
  WireGuard pod overlay (Linux needs the `wireguard` module loaded).
- The default cluster is `dev` (context `litek8s-dev`, port 6443, socket
  `~/.litek8s/docker.sock`). It is usually the user's own: do not stop or
  delete it unless asked.
- `dev up` makes the new context current unless `--no-switch`; the MCP tool
  never switches unless `switch_context: true`.

## What each cluster gets

| | default `dev` | `--name N` |
|---|---|---|
| kube context | `litek8s-dev` | `litek8s-N` |
| API | `http://127.0.0.1:6443` (free one if taken) | `http://127.0.0.1:<port>` (remembered) |
| Docker socket | `~/.litek8s/docker.sock` | `~/.litek8s/N/docker.sock` |
| NodePorts on localhost | from 30000 | 30100-32700, by name |

`cluster_status` / `dev status -o json` give the exact `server` and
`docker_socket`.

## Images

- `litek8s dev load --name N image:tag` exports from the local Docker or
  Podman and imports; loaded images are never pulled over (`:latest` works
  without `imagePullPolicy: IfNotPresent`).
- Or build straight into the cluster's store:
  `DOCKER_HOST=unix://$HOME/.litek8s/docker.sock docker build -t myapp:dev .`
  (named cluster: `~/.litek8s/N/docker.sock`), then use `image: myapp:dev`.
- Image stores are per cluster.
- A registry on the host's localhost works (`image: localhost:5000/x:1`).
- Private registries, CAs, mirrors: `~/.litek8s/registries.yaml` (k3s
  format), read by `dev up` at each start (docs/registries.md).

## Settings

`~/.litek8s/config.yaml` (edited by `litek8s ui`): `cpus`, `memory` (MiB,
the macOS VM's ceiling, default 4096), `shares` (macOS: directories shared
at the same path, default `$HOME`), `port`, `publish` (`localhost` or
`all`). They apply when the first cluster starts the VM/sandbox.

## Reaching workloads

- NodePort and LoadBalancer Services listen on the host's localhost.
- `kubectl --context litek8s-N port-forward svc/x 8080:80` works.
- hostPorts are not exposed on the host.

## Limits

- No authentication on the API: it listens on 127.0.0.1 only.
- All clusters share the VM's CPUs/memory (no per-cluster limits yet).
- Rootless Linux cannot bind host ports below 1024.
