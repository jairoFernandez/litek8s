---
name: litek8s
description: Operate litek8s, a Kubernetes API server in one Rust binary with local dev clusters (`litek8s dev`, a kind/k3d replacement without Docker), a built-in Docker Engine API, built-in metrics/logs/alerts (PromQL, log search), HA over Raft and joining nodes. Use when a kube context is litek8s-*, the user mentions litek8s, `litek8s dev`, ~/.litek8s/docker.sock, or wants a local cluster, to debug a pod there, to query its metrics or logs, or to run Docker containers on it.
---

# litek8s

litek8s serves the real Kubernetes API (`/version` says `v1.33.0+litek8s`;
conformance 413/419), so **kubectl, Helm and everything you know about
Kubernetes objects work unchanged**. What kubectl does not cover is
litek8s's own surface: dev clusters, the Docker socket, metrics/logs/alerts
without Prometheus or Loki, HA and node join. Use the litek8s tools (MCP
server `litek8s mcp`) or the `litek8s` CLI for those, kubectl for the rest.

## Pick the tool

| want | use |
|---|---|
| a local cluster: create, start, stop, delete, nodes | `cluster_up` / `cluster_down` / `cluster_delete`, or `litek8s dev up|down|delete --name N [--nodes K]` |
| what clusters exist, is it running, where | `clusters_list`, `cluster_status`; `litek8s dev list -o json`, `litek8s dev status --name N -o json` |
| a pod is not Ready / CrashLoopBackOff / Pending | `diagnose_pod` first (conditions, container states, last exit, probes, node, events, current and previous logs in one call) |
| objects | `get` (table, or one object with events and owner chain), `describe`, `events`; or kubectl |
| deploy | `apply` (server-side apply; `dry_run`), or `kubectl apply` |
| logs of one pod | `pod_logs` (`previous: true` after a restart), or `kubectl logs` |
| logs across pods, namespaces, deleted containers | `logs_search` (`grep`, `since`, `selector`), or `litek8s logs -l app=x --grep ERROR --since 1h -o json` |
| CPU, memory, restarts, network over time | `query` / `query_range` (PromQL), or `litek8s query '...' -o json`; recipes: references/observability.md |
| what is wrong right now | `alerts_list`; `litek8s alerts -o json` |
| GitOps: Argo CD Applications (run by Argo CD or litecd) synced? why OutOfSync or Degraded? sync one | `applications_list`, `application_status` (the resources that need attention, with messages), `application_diff` (live against git, per resource), `application_sync` (`confirm: true`; `prune` deletes what git dropped), `application_refresh` |
| GitOps on a small cluster (Argo CD's manifests in one ~10 MiB pod instead of Argo CD's ~300 MiB) | litecd: `curl -fsSL https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh \| KUBECTL="kubectl --context litek8s-N" sh` (asks nothing; next to an existing Argo CD it installs only litecd and stands down until Argo CD's controller is scaled to 0); a whole demo: `examples/litecd/up.sh`; install, moving over from Argo CD, numbers: `litek8s://docs/litecd.md` |
| Docker containers/images on a dev cluster | `docker_ps`, `docker_images`, `docker_logs`; or `DOCKER_HOST=unix://$HOME/.litek8s/docker.sock docker ...` |

## Rules

- **Only litek8s.** The MCP tools refuse a context whose server is not
  litek8s. Do not point kubectl at other contexts to "help": a kubeconfig
  can also hold production clusters. Always name the context you mean.
- **Contexts:** dev cluster `N` is `litek8s-N` (default cluster `dev` is
  `litek8s-dev`). `cluster_up` adds the context without switching the
  current one; pass `context: "litek8s-N"` to the tools, or
  `kubectl --context litek8s-N`.
- **Destructive calls need `confirm: true`** (`delete`, `cluster_down`,
  `cluster_delete`). Call without it first: the answer says what would go.
  Ask the user before confirming anything that deletes their data. Never
  delete a cluster you did not create in this task.
- **Scratch work** goes in its own cluster: `cluster_up name=<task>`, then
  `cluster_delete name=<task> confirm=true` when done. A cluster costs a few
  MiB and starts in ~1-2 s once the VM/sandbox runs.
- **Docker: name the socket on every command**
  (`DOCKER_HOST=unix://$HOME/.litek8s/N/docker.sock docker ...`). An
  `export` in one shell call does not reach the next, and the command then
  hits the machine's own Docker; check with `docker version --format
  '{{.Server.Platform.Name}}'` (says `litek8s`). references/docker.md.
- **Big results are cut** (`_truncated` with a hint): narrow with
  namespace, label_selector, grep, since, limit rather than asking again.

## Workflows

**Debug a broken workload.**
1. `get kind=pods namespace=X label_selector=app=Y` → find the not-ready pod.
2. `diagnose_pod namespace=X pod=P` (or `selector=app=Y`) → read
   `findings[0]` (cause and next step), then the evidence (`containers`,
   `events`, `logs.<container>.previous_logs`).
3. Fix the manifest, `apply`, then `diagnose_pod` again or
   `get kind=deployments name=Y` (owner status `ready n/m`).

Typical causes and their signal: image missing → `ImagePullBackOff`
(local image: `litek8s dev load --name N image:tag`); app crashes →
`CrashLoopBackOff` + exit code + previous logs; OOM → last exit `OOMKilled`
(137); bad config ref → `CreateContainerConfigError` naming the
ConfigMap/Secret; probe → `ReadinessProbeFailing` with the probe (litek8s
records no probe output: check the port/path against the app's logs);
unschedulable → `PodScheduled=False` message (resources, nodeSelector,
taints). More: references/troubleshooting.md.

**Try something on a fresh cluster.**
`cluster_up name=try nodes=1` → `apply` → `get`/`diagnose_pod` →
`cluster_delete name=try confirm=true`.

**Multi-node behavior** (spreading, DaemonSets, node failure):
`cluster_up name=multi nodes=3`; stop a worker with `cluster_down
name=multi node=litek8s-multi-worker2 confirm=true` (NotReady after ~40 s,
pods evicted), bring it back with `cluster_up name=multi
node=litek8s-multi-worker2`.

**"Is it slow / big / restarting?"** `query` with the recipes in
references/observability.md, e.g. top memory pods:
`topk(10, sum by (namespace, pod) (container_memory_working_set_bytes{container!=""}))`.

## References (read when needed)

- references/dev-clusters.md: dev cluster lifecycle, names, ports, images, settings, limits
- references/observability.md: metric names, PromQL recipes, log search, alerts and silences
- references/docker.md: the Docker API: compose, builds, what is unsupported
- references/ha-and-join.md: 3/5-server control plane, `litek8s agent --join`, `litek8s lb`
- references/auth-and-rbac.md: servers' authentication and RBAC: 401/403 after upgrading, audit mode, fixes
- references/differences.md: where litek8s differs from upstream Kubernetes
- references/troubleshooting.md: symptoms → causes for pods, nodes, dev clusters, the API

`litek8s skill <name>` prints any of them; the MCP server serves them as
`litek8s://skill/references/<name>.md` and the full docs as
`litek8s://docs/<file>.md`.
