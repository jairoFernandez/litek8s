# Troubleshooting litek8s

Start with `diagnose_pod` for any pod problem: it collects what you would
otherwise get from `kubectl describe pod`, `kubectl logs --previous`,
`kubectl get node` and the events, and turns it into findings.

## Pods

| symptom (finding reason) | cause | what to do |
|---|---|---|
| `ErrImagePull` / `ImagePullBackOff` | wrong name/tag, private registry, or a local image the cluster does not have | fix the tag; private registry (unknown CA, `requires authentication`): the nodes' registries.yaml (docs/registries.md; `imagePullSecrets` are not used); local image: `litek8s dev load --name N img:tag` or `docker build` on the cluster's socket |
| `CrashLoopBackOff` + exit 1 | the app fails at start | `previous_logs` in diagnose_pod (or `pod_logs previous=true`) |
| exit 137 `OOMKilled` | memory limit too low | raise `resources.limits.memory`; check `container_memory_working_set_bytes` |
| exit 137 without OOM, liveness probe set | liveness probe killed it | check the probe's port/path/timeout and the app's startup time; add a startupProbe |
| exit 127 / 126 | command not found / not executable | fix `command`/`args` or the image |
| exit 0, restarting | a long-running container exited | the process must stay in the foreground |
| `CreateContainerConfigError` | a referenced ConfigMap/Secret (or key) is missing | create it; `get kind=configmaps namespace=X` |
| `ReadinessProbeFailing` | running but the probe fails | the app listens elsewhere, the path errors, or the timeout is short; litek8s records no probe output: compare with logs, `kubectl exec ... -- wget -qO- localhost:PORT/path` |
| `Unschedulable` | no node fits: requests, nodeSelector, affinity, taints | `get kind=nodes`; `describe kind=node name=...`; dev: `cluster_up nodes=N` |
| `NotScheduled` with no condition | no Ready nodes | `get kind=nodes`; dev cluster: `cluster_status` |
| `NodeNotReady` | the node's agent stopped heartbeating | dev: `cluster_up name=N node=<node>`; real node: is `litek8s agent` running, network/WireGuard? |
| `ContainerCreating` for minutes | slow pull, or a volume that cannot mount (PVC not Bound, missing Secret) | events; `get kind=pvc` |
| `Terminating` forever | finalizers, or its node is gone | `get` the object's `metadata.finalizers`; node lifecycle evicts pods of dead nodes |
| Ready, but the Service does not answer | selector does not match, wrong targetPort | `describe kind=service` (endpoints); `get kind=endpoints name=svc` |

## Dev clusters

- `cluster_status` says not running → `cluster_up name=N`.
- `cluster_up` fails or times out → its `output`/`stderr` tail; the
  cluster's log is `dev.log` in its data directory (`cluster_status`);
  the VM/sandbox log is `env.log` in the dev home
  (`~/Library/Application Support/dev.litek8s/dev` on macOS,
  `~/.local/share/litek8s/dev` on Linux).
- Linux multi-node: workers not joining → `wireguard` module not loaded
  (`sudo modprobe wireguard`); `litek8s dev setup` checks prerequisites.
- Port taken → a named cluster keeps its last port; the default takes a
  free one when 6443 is busy (see `cluster_status.server`).
- A NodePort not reachable on localhost: another cluster holds the same
  port (its log says "not exposed").
- Docker results that contradict each other (`created` yet its port
  answers, a container missing from `ps`, `port is already allocated`,
  a call that seems to hang): first make sure every command went to
  litek8s. Put `DOCKER_HOST=unix://...` on each command (an `export` does
  not survive between shell calls) and check `docker version --format
  '{{.Server.Platform.Name}}'` says `litek8s`. The machine's own Docker
  may hold the same host ports (`docker ps` with `DOCKER_HOST` unset).

## The API server / tools

- "refusing: ... is not a litek8s API server" → you passed a non-litek8s
  context. Use `contexts_list probe=true` to find litek8s ones; use
  kubectl for other clusters.
- "cannot reach ..." → cluster stopped, or wrong context.
- 401 `Unauthorized` / `whoami` says `system:anonymous` on a server → a
  context from an earlier litek8s (placeholder token): upgrade the CLI and
  re-run `litek8s cluster apply` / `install --ssh`; 403 `Forbidden` from a
  pod → its service account lacks a Role. references/auth-and-rbac.md.
- `logs_search` / `query` say there is no API → the server runs with
  `--no-observability`.
- Empty PromQL result → the metric needs a sample (every 10 s) or the
  matchers are wrong; list names with `count by (__name__) ({__name__=~"container_.*"})`.
- Memory of the dev VM: `litek8s dev status` (and `litek8s ui` Home).
