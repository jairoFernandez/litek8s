# Compatibility results

Corpus: 74 cases in 5 tiers (see [README.md](README.md)). Run on 2026-10-16
with kubectl v1.33.9 and Helm v4.3.0 on an Apple M3 Max.

| tier | k3s (k3d, reference) | litek8s apiserver (2026-10-16) | litek8s apiserver + controllers + node agent (2026-10-17) |
|---|---|---|---|
| L0 discovery | 7/7 | 7/7 | **7/7** |
| L1 object API | 25/25 | 25/25 | **25/25** |
| L2 workloads | 15/15 | 0/15 | **15/15** |
| L3 networking | 7/7 | 0/7 | not rerun |
| L4 Helm charts (`--wait`) | 20/20 | 0/20 | **16/20** |
| **all** | **74/74 (100 %)** | **32/74 (43 %)** | **63/74 (85 %)** with L3 at its last score |

- **Reference.** k3s v1.36.5+k3s1 via k3d (1 server + 1 agent, traefik and
  metrics-server disabled because L4 installs them). Every case passes, so the
  corpus itself is sound. Two cases (`api_versions`, `list_all_namespaces`)
  were fixed during the run, for a SIGPIPE race between `kubectl | grep -q` and
  `pipefail`. Their results come from a recheck on the same cluster and are
  merged into `results/20261016-202102-k3d-baseline/`.
- **litek8s, 2026-10-17.** `litek8s apiserver` (release build, in-memory
  store) now runs the workload controllers, a scheduler and a Docker-backed
  node agent in the same process, on OrbStack (Docker 29.8) on an M3 Max.
  L2: `results/20261016-223540-litek8s-L2/`, `--max-case-seconds 120`; 13 of
  the 15 cases finish in about a second. L4 with `COMPAT_HELM_TIMEOUT=3m/4m`,
  in two runs: `results/20261017-102203-litek8s-L4a/` (podinfo through
  argo-cd; the run was cut off after argo-cd) and
  `results/20261017-103921-litek8s-L4b/` (the rest, plus a metrics-server
  rerun after the kubelet endpoint landed).
- **L4 still failing (4):** external-secrets and cnpg (their webhook
  certificates arrive later through a Secret, and the agent wrote Secret
  volumes only once), gitea (an init script mounted with `defaultMode: 0110`
  needs `fsGroup` group ownership), loki (its gateway's nginx resolves
  `kube-dns.kube-system.svc.cluster.local` at startup). Fixes for all three
  causes are implemented but not yet scored: Secret/ConfigMap volume refresh
  pushed from the API server, `fsGroup` ownership, and cluster DNS
  (`src/kube/dns.rs`).

## What made L2 and L4 pass

- **Controllers in-process, on the store's change feed** (`src/kube/controllers/`):
  ReplicaSet, Deployment (RollingUpdate/Recreate, revisions, `rollout undo`),
  Job (incl. Indexed, back-off, TTL), CronJob, StatefulSet (ordinals,
  OrderedReady, claim templates), DaemonSet, ownerReference GC, a scheduler,
  local-path volume provisioning, Endpoints/EndpointSlices and LoadBalancer
  status. One task, no polling: changes are batched and reconciled against
  the in-memory store through the same defaulting the HTTP handlers apply.
- **A node agent over one persistent connection** (`src/kube/agent/`): pods
  are pushed to it, status comes back on the same connection, and
  log/exec/port-forward streams are multiplexed over it. The local node runs
  in-process over a pipe; `litek8s node-agent --server ...` dials in with one
  HTTP upgrade. Pods are a sandbox container owning the network namespace
  plus containers joined to it, with init and sidecar containers, probes,
  restart back-off, emptyDir/PVC/configMap/secret/projected volumes.
- **Streaming subresources**: `pods/log` (follow), `exec`/`attach` over
  WebSocket (`v5.channel.k8s.io`), and `port-forward` over SPDY/3.1, both
  tunneled in a WebSocket (kubectl 1.31+) and as a plain upgrade.
- **What charts needed beyond running pods**: TLS on the API port for
  in-cluster clients (with service account tokens and `kube-root-ca.crt`),
  mutating/validating admission webhooks (cert-manager's startup check),
  metadata-only watches (`as=PartialObjectMetadata`), a `kubelet`
  `/metrics/resource` endpoint (metrics-server), token/access reviews, the
  `extension-apiserver-authentication` ConfigMap, and no-op updates that do
  not bump `resourceVersion` (cert-manager's cainjector otherwise rewrote its
  webhook configurations ~1,200 times a second).

## API server footprint

Release build, macOS arm64, process RSS:

| state | RSS |
|---|---|
| idle, API only (default namespaces, service accounts, `kubernetes` Service) | 9.7 MiB |
| + 1,000 ConfigMaps (~400 B of data each) | 26.9 MiB |
| + 5 watchers streaming those ConfigMaps | 41.9 MiB |
| idle, with controllers and the local node agent | 15.8 MiB |
| **+ 5 running pods** (`kubectl create deployment --replicas=5`) | **24.7 MiB** |

- With 5 running pods the process (API server, controllers, scheduler and
  node agent) idles at 0.0 % CPU in `top` over 20 s. The agent reacts to
  Docker events and the API server's push feed; it does not poll.
- `kubectl create deployment --replicas=5` to `rollout status` done: 0.6 s
  with the image cached.
- `kubectl apply` of the 1,000 ConfigMaps took 0.48 s. A raw list of all
  1,000 took 8 ms. The WAL was 1.4 MB.
- For scale only (the two do different work): k3s-server is 400 MiB PSS at
  idle in `bench/RESULTS.md`, and that single process also runs the
  scheduler, controllers, kubelet and kube-proxy.
- Watchers copy their initial list (about 3 MiB each here). Storing objects
  once as shared serialized bytes is the obvious next optimization.

## What made L0–L1 pass

Found by running real kubectl against the server, not from the spec:

- **kubectl sends protobuf** for built-in types on `create`/`run`/`expose`,
  with no opt-out. The server decodes it using the vendored Kubernetes
  `.proto` schemas (`proto/`, `src/kube/protobuf.rs`). Responses stay JSON,
  which every client accepts.
- **Aggregated discovery** (`apidiscovery.k8s.io/v2`): kubectl discovers
  everything in 2 requests instead of one per group version.
- **`/openapi/v3`** with paths that advertise `fieldValidation`, so kubectl
  leaves validation to the server instead of downloading schemas. For
  `kind: List` kubectl falls back to `/openapi/v2` in protobuf, which gets a
  valid empty document.
- **Server-side Table printing**, so `kubectl get` shows the real columns, and
  watch events as Table rows for `kubectl get -w`.

## Next, by score impact

1. Finish the three L4 fixes above (volume refresh, `fsGroup`, cluster DNS).
2. L3: service proxy for ClusterIPs and NodePorts, cluster DNS, pod-to-pod.
3. Authentication and authorization (today every client is admin), and
   admission webhooks for writes made by the in-process controllers.
