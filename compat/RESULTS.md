# Compatibility results

Corpus: 74 cases in 5 tiers (see [README.md](README.md)). Run on 2026-10-16
with kubectl v1.33.9 and Helm v4.3.0 on an Apple M3 Max.

| tier | k3s (k3d, reference) | litek8s apiserver, API only (2026-10-16) | litek8s apiserver + controllers + node agent (2026-10-17) |
|---|---|---|---|
| L0 discovery | 7/7 | 7/7 | **7/7** |
| L1 object API | 25/25 | 25/25 | **25/25** |
| L2 workloads | 15/15 | 0/15 | **14/15** |
| L3 networking | 7/7 | 0/7 | **6/7** |
| L4 Helm charts (`--wait`) | 20/20 | 0/20 | **20/20** |
| **all** | **74/74 (100 %)** | **32/74 (43 %)** | **72/74 (97 %)** |

- **Reference.** k3s v1.36.5+k3s1 via k3d (1 server + 1 agent, traefik and
  metrics-server disabled because L4 installs them). Every case passes, so the
  corpus itself is sound. Two cases (`api_versions`, `list_all_namespaces`)
  were fixed during the run, for a SIGPIPE race between `kubectl | grep -q` and
  `pipefail`. Their results come from a recheck on the same cluster and are
  merged into `results/20261016-202102-k3d-baseline/`.
- **litek8s, 2026-10-17.** One run of the whole corpus,
  `results/20261017-114819-litek8s-full/`: `litek8s apiserver` (release build, in-memory store)
  with its controllers, scheduler, cluster DNS and the local Docker-backed
  node agent in the same process, on OrbStack (Docker 29.8) on an M3 Max.
  `--max-case-seconds 300`, `COMPAT_HELM_TIMEOUT=4m`. Most L2 and L3 cases
  finish in about a second; the slowest chart (kyverno) in under a minute.
- **Not passing (2):**
  - `port_forward` (fixed since): it failed in the full run and in some
    reruns. The agent's first connection to a pod created a moment earlier
    could be refused (nginx not listening yet) or go unanswered (the host's
    route to a brand-new pod address under OrbStack), and kubectl 1.33 ends
    the whole port-forward on the first stream error. The agent now retries
    such connections for up to 5 s; L2 has passed 15/15 since.
  - `api_from_pod`: the case uses busybox's built-in TLS client, which
    aborts the handshake with the API server's rustls TLS stack. In-cluster
    clients built on Go or OpenSSL connect fine (every L4 chart with an
    operator talks to the API this way).

## What made L2, L3 and L4 pass

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
- **Networking**: cluster DNS served by the API server process
  (`src/kube/dns.rs`: Services resolve to their ClusterIP, headless ones to
  ready pod IPs, the rest is forwarded upstream), and a service proxy in the
  node agent (`src/kube/agent/proxy.rs`): ClusterIPs as one iptables NAT chain
  loaded with `iptables-restore` through a small privileged helper container
  in Docker's host network namespace (so it also works when Docker runs in a
  VM), NodePorts as listeners on the node. The API is also served on port 443
  of the listen address for `https://kubernetes.default.svc`.
- **What charts needed beyond running pods**: TLS on the API port for
  in-cluster clients (with service account tokens and `kube-root-ca.crt`),
  mutating/validating admission webhooks (cert-manager's startup check),
  metadata-only watches (`as=PartialObjectMetadata`), a `kubelet`
  `/metrics/resource` endpoint (metrics-server), token/access reviews, the
  `extension-apiserver-authentication` ConfigMap, and no-op updates that do
  not bump `resourceVersion` (cert-manager's cainjector otherwise rewrote its
  webhook configurations ~1,200 times a second), Secret and ConfigMap
  volumes that refresh when the object changes and use the kubelet's
  `..data` symlink layout (external-secrets and cnpg wait for webhook
  certificates that arrive later; gitea's init script looks for symlinks),
  `fsGroup` ownership, and `spec.conversion` defaulted on CRDs.

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

1. Authentication and authorization (today every client is admin), and
   admission webhooks for writes made by the in-process controllers.
2. A TLS stack that also accepts busybox's minimal client (`api_from_pod`).
3. Service proxy without the helper container on Linux hosts, where the
   agent can program the host's NAT table directly.
