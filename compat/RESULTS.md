# Compatibility results

Corpus: 74 cases in 5 tiers (see [README.md](README.md)). Run on 2026-10-16
with kubectl v1.33.9 and Helm v4.3.0 on an Apple M3 Max.

| tier | k3s (k3d, reference) | litek8s apiserver |
|---|---|---|
| L0 discovery | 7/7 | **7/7** |
| L1 object API | 25/25 | **25/25** |
| L2 workloads | 15/15 | 0/15 |
| L3 networking | 7/7 | 0/7 |
| L4 Helm charts (`--wait`) | 20/20 | 0/20 |
| **all** | **74/74 (100 %)** | **32/74 (43 %)** |
| *L4 without `--wait` (API accepts every object)* | – | *17/20* |

- **Reference.** k3s v1.36.5+k3s1 via k3d (1 server + 1 agent, traefik and
  metrics-server disabled because L4 installs them). Every case passes, so the
  corpus itself is sound. Two cases (`api_versions`, `list_all_namespaces`)
  were fixed during the run, for a SIGPIPE race between `kubectl | grep -q` and
  `pipefail`. Their results come from a recheck on the same cluster and are
  merged into `results/20261016-202102-k3d-baseline/`.
- **litek8s.** `litek8s apiserver`, release build, WAL on disk, run with
  `--max-case-seconds 45` and `COMPAT_HELM_TIMEOUT=40s` (a quick score;
  without controllers, L2–L4 can only time out).
- **The 0 % in L2–L4 is expected.** The API server stores Deployments, Pods
  and Services, but no controller or node agent acts on them yet. Without
  `--wait`, Helm installs 17 of the 20 charts: the API side of CRDs, RBAC,
  webhook configurations, Services and workloads is accepted. The 3 that fail
  (cert-manager, kyverno, argo-cd) run Helm hook Jobs and wait for them to
  complete, which needs the Job controller.

## API server footprint

Release build, macOS arm64, in-memory process RSS:

| state | RSS |
|---|---|
| idle (default namespaces, service accounts, `kubernetes` Service) | 9.7 MiB |
| + 1,000 ConfigMaps (~400 B of data each) | 26.9 MiB |
| + 5 watchers streaming those ConfigMaps | 41.9 MiB |

- `kubectl apply` of the 1,000 ConfigMaps took 0.48 s. A raw list of all
  1,000 took 8 ms. The WAL was 1.4 MB.
- The binary is 20.8 MiB, including the embedded protobuf descriptors.
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

1. Workload controllers (ReplicaSet/Deployment, Job, StatefulSet, DaemonSet,
   CronJob) plus a node agent that runs Pods from the object store. These
   unlock L2 and most of L4.
2. Pod `log`/`exec`/`portforward` subresources (SPDY/WebSocket upgrades).
3. Service proxy (nftables), cluster DNS and EndpointSlices, which unlock L3.
4. TLS and ServiceAccount tokens (`api_from_pod`, charts that call the API),
   then admission webhooks.
