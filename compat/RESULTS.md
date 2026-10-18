# Compatibility results

Corpus: 101 cases in 5 tiers (see [README.md](README.md)); L4 grew from 20
to 47 Helm charts on 2026-10-17. Run on 2026-10-17 with kubectl v1.33.9 and
Helm v4.3.0 on an Apple M3 Max, Docker 29.8.2 under OrbStack.

| tier | k3s (k3d, reference) | litek8s, 74-case corpus (main, e8a5de6) | litek8s, 101-case corpus (2026-10-17) | litek8s on crun, arm64 VM | litek8s on crun, x86_64 |
|---|---|---|---|---|---|
| L0 discovery | 7/7 | 7/7 | **7/7** | **7/7** | **7/7** |
| L1 object API | 25/25 | 25/25 | **25/25** | **25/25** | **25/25** |
| L2 workloads | 15/15 | 14/15 | **15/15** | **15/15** | **15/15** |
| L3 networking | 7/7 | 6/7 | **7/7** | **7/7** | **7/7** |
| L4 Helm charts (`--wait`) | 47/47 | 20/20 (of the first 20) | **47/47** | **47/47** | **47/47** |
| **all** | **101/101 (100 %)** | **72/74 (97 %)** | **101/101 (100 %)** | **101/101 (100 %)** | **101/101 (100 %)** |

- **Reference.** k3s v1.33.4+k3s1 via k3d (1 server + 1 agent, traefik and
  metrics-server disabled because L4 installs them),
  `results/20261017-143430-k3d-baseline/`. Every case passes, so the corpus
  is sound. Three L4 cases come from a recheck on the same cluster, merged
  into that directory: `helm_headlamp` (its helm process was killed by
  mistake during the run), `helm_alloy` (a chart download from GitHub
  failed) and `helm_home_assistant` (the 1.5 GB image did not pull within
  the 6 min timeout on the node it landed on).
- **litek8s.** One run of the whole corpus,
  `results/20261017-151522-litek8s-full/`: `litek8s apiserver` (release
  build, in-memory store) with its controllers, scheduler, cluster DNS and
  the local Docker-backed node agent in the same process.
  `--max-case-seconds 420`. Then L2+L3 three more times
  (`results/*-litek8s-l2l3-rerun{1,2,3}/`, 22/22 each), and L0–L3 with the
  `--no-default-features` build (`results/*-litek8s-minimal-build/`, 54/54).
- **litek8s on crun.** The same `litek8s apiserver` with `--runtime crun`:
  pods run with crun directly, no Docker (see `bench/RESULTS.md`).
  `results/20261017-164402-litek8s-crun-arm64/`: Ubuntu 24.04 arm64 qemu
  VM (4 vCPU, 4 GiB, 12 GB disk, so image GC ran during L4), 32.2 min.
  `results/20261017-164402-litek8s-crun-amd64/`: x86_64 Linux workstation,
  the node inside a privileged Ubuntu 24.04 container (own network
  namespace, mounts and cgroup subtree), 21.9 min. Both pulled every
  image cold through the node's own registry client; the slowest chart
  was Home Assistant on arm64 (228 s, a 1.5 GB zstd image).
- **Time.** The whole corpus took 12.3 min on litek8s (L4: 11.4 min, the
  slowest chart pihole at 62 s) against 50.6 min on k3s (L4: 47 min). The
  27 new charts alone: 6.0 min against 26.3 min. The k3s run shared the
  Docker VM with litek8s runs for part of the time, and image caches
  differ, so take the ratio as rough.
- **The 27 charts added** (`charts.txt`): kube-prometheus-stack,
  VictoriaMetrics single, Grafana Alloy, HAProxy ingress, Envoy Gateway,
  NGINX Inc. ingress, MetalLB, OpenEBS local PV (hostpath), OT redis-operator,
  mariadb-operator, Zalando postgres-operator, Valkey, Home Assistant,
  Jellyfin, Nextcloud, Uptime Kuma, Vaultwarden, Pi-hole, Node-RED,
  Homepage, Gatus, zot, the CNCF Docker registry, Flux, Argo Workflows,
  descheduler, Trivy operator. Tried and left out because they fail on k3s
  too: fluent-bit, authelia, immich, ollama (see `charts.txt`).

## What the 27 new charts exposed (fixed)

- **Protobuf from client-go**: `SelfSubjectAccessReview` (prometheus-operator)
  and `CustomResourceDefinition` (haproxy-ingress's CRD job, zalando
  postgres-operator) arrive as protobuf. Vendored the authorization,
  authentication and apiextensions schemas; JSONSchemaProps needs its
  hand-written JSON forms (`$ref`, `x-kubernetes-*`, schema-or-bool unions).
- **ValidatingAdmissionPolicy(+Binding)** served (Envoy Gateway ships one;
  the CEL rules are stored, not evaluated), and RuntimeClass.
- **Concurrent Service creates got the same ClusterIP** (and could get the
  same nodePort): cert-manager's webhook and cainjector Services shared an
  address. Allocations are now serialized and reserved until stored.
- **Serving certificate without an authority key identifier**: Python 3.13's
  strict verification (Grafana's k8s-sidecar) rejected it.
- **Probes**: probe `httpHeaders` now replace the default `Host` (Nextcloud
  answered the duplicate with 400); a hostNetwork pod's `127.0.0.1` is the
  Docker VM's loopback, not the node's (metallb).
- **Pods**: `shareProcessNamespace` and `hostPID` (metallb's frr reloader is
  signalled by PID); memory `emptyDir` is world-writable (haproxy binds a
  socket in a `/run` emptyDir as uid 1000); image `VOLUME`s get named
  volumes instead of Docker's anonymous ones, which vaultwarden refuses as
  "not persistent".

## What the crun runtime needed for 101/101

- **zstd layers**: Home Assistant's image is `tar+zstd`; the store only
  knew gzip. Layers are now decoded with ruzstd (pure Rust), frame by
  frame, skipping skippable frames (zstd:chunked's table of contents).
- **busybox TLS on x86_64**: busybox's client lists P-256 first and rustls
  takes the client's order, but busybox's x86_64 P-256 code sends invalid
  key shares (rustls `InvalidKeyShare`, alert 47), so `api_from_pod` failed
  on amd64 nodes only (arm64 passed). Clients that offer X25519 now get it.
- **Images with a repeated layer** (argo-cd): overlayfs refuses
  overlapping lower directories; only the topmost copy is kept.
- **Stale namespace files after a node restart** where /run is not a
  tmpfs: a sandbox counts as running only while its namespace files are
  real nsfs mounts.
- **`shareProcessNamespace` / `hostPID`**: no PID namespace entry for
  hostPID; for a shared one, a tiny `litek8s crun-pause` holds the pod's
  PID namespace as PID 1 (only in pods that ask for it).
- The Docker-in-a-VM fallbacks (`nc`/`wget` in the sandbox) are skipped:
  a crun node always reaches its pods directly.

## The two failures on main

- **`api_from_pod`**: busybox's built-in TLS client (`wget` in busybox
  images) expects one TLS handshake message per record. rustls sends its
  whole first TLS 1.2 flight (ServerHello, Certificate, ServerKeyExchange,
  ServerHelloDone) as one record, and busybox stalled after the
  ServerHello. Versions, cipher suites, key type and certificate were all
  fine: busybox offered TLS 1.2 with ECDHE-ECDSA-AES128-GCM and P-256/X25519,
  which rustls accepts. The API server's TLS transport now re-frames the
  plaintext handshake records it writes, one message per record (valid TLS
  for every client; OpenSSL still negotiates TLS 1.3, defaults unchanged).
- **`port_forward`, and an intermittent `http_readiness`**: with Docker in a
  VM, the host's route to a pod address is sometimes missing for a while
  (OrbStack: the VM reaches the pod, macOS does not; Docker Desktop has no
  route at all), so anything the node or API server dials at a pod address
  can hang. Probes, port-forward, NodePort relays and admission webhook
  calls now try the address directly first, and when that neither
  connects nor is refused they go through the pod's sandbox (`nc`/`wget`
  run in the pod's network namespace), or for webhooks through the node
  agent that runs the pod. The address is remembered so later connections
  skip the timeout. This was reproduced by dropping host traffic inside one
  pod with iptables: the pod still turned Ready and port-forward, NodePort
  and webhook calls still worked.

## Binary size

Release builds on macOS arm64 (`opt-level = 3`, unstripped):

| build | size |
|---|---|
| main (e8a5de6) | 26.5 MiB (27,789,328 B) |
| default features (`llm` on) | 26.7 MiB (27,969,152 B) |
| `--no-default-features` (no LLM/tensor-model stack) | **21.7 MiB** (22,725,632 B) |

The `llm` feature gates safetensors/GGUF loading, tokenizers (oniguruma),
the transformer, KV cache and distributed LLM coordinator, and the `llm`
and `model` commands. Distributed matmul stays, since the daemon and node
agent protocol carry it. `strip` would take another ~4 MiB off either build.

## What made L2, L3 and L4 pass on main

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
2. Evaluate CEL: ValidatingAdmissionPolicy rules and webhook
   `matchConditions` are accepted but not enforced.
3. A memory `emptyDir` is one tmpfs per container mount today; Kubernetes
   shares one per pod between its containers.
4. Service proxy without the helper container on Linux hosts, where the
   agent can program the host's NAT table directly.

## Multi-node (2026-10-17)

A 3-node litek8s cluster on local VMs (`bench/cluster.sh up 3`: Ubuntu
24.04 arm64, 2 vCPU / 1 GiB each, crun, pods over the WireGuard overlay;
lk1 runs the API server and is a node, lk2 and lk3 joined with
`litek8s agent --join`), and the new **M1** tier of cross-node cases
(`compat/cases/M1-multinode.sh`, not in the default tiers, so the corpus
stays at 101 cases): pod-to-pod across nodes with source addresses kept, a
DaemonSet on every node reachable by IP and DNS, a Service balancing over
endpoints on two nodes, a Deployment surviving a worker's hard power-off
(evicted, rescheduled, clean rejoin), and a short outage that keeps its pod.

| tier | k3s (k3d, 1 server + 2 agents) | litek8s, 3 VMs |
|---|---|---|
| M1 multi-node | 5/5 | **5/5** |
| L2 workloads | | **15/15** |
| L3 networking | | **7/7** |
| L4 Helm charts | | **47/47** |

- M1 litek8s: `results/20261017-205817-litek8s-3vm/`, power commands
  `bench/vm.sh kill|start`. k3s: `results/20261017-211854-k3d-multi/` (4/5;
  `multi_service_spreads` raced kube-proxy's rule sync, the case now retries)
  then `20261017-212137-k3d-multi/` (pass); `211907` failed that case only
  because the previous run's namespace was still terminating. k3d nodes are
  killed with `docker kill|start`; the kubelet's disk eviction thresholds were
  lowered because OrbStack's disk was 94 % full.
- L2–L4 litek8s: `results/20261017-211147-litek8s-3vm/` (L2, L3 and 23 of
  47 charts: the shared Helm repository cache was clobbered by another
  process during the run), `20261017-213456-litek8s-3vm-l4rerun/` (with a
  private Helm cache; stopped after lk1's 12 GB disk filled: failed layer
  unpacks left their partial directories behind, fixed in the image store)
  and `20261017-220217-litek8s-3vm-l4rerun2/` for the rest. Every chart
  passed once; L4 was not repeated as a single clean run on 3 VMs.

