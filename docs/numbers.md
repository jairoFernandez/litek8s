# Numbers

Measured against k3s, kind and k3d with the scripts in this repository; the
linked files have the setup, raw data and caveats.

| | litek8s | reference | source |
|---|---|---|---|
| Whole control plane, idle, no pods (PSS, v0.2.1, 1 GiB x86_64 VM) | **23.2 MiB** | k3s minimal: 567 MiB | [bench/RESULTS.md](../bench/RESULTS.md#footprint-of-the-v021-release-2026-10-21-x86_64kvm) |
| Whole orchestrator, 5 nginx pods (same) | **33.5 MiB** | k3s minimal: 579 MiB | same |
| Argo CD + one synced app, orchestrator memory | **90 MiB** | k3s minimal: 900 MiB | [bench/RESULTS.md](../bench/RESULTS.md#argo-cd-on-k3s-and-on-litek8s-2026-10-19-x86_64kvm) |
| `dev up` → node Ready, Linux / macOS | **0.17 s / 1.28 s** | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s | [bench/RESULTS.md](../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18) |
| Dev cluster idle memory, Linux | **47 MiB** | kind 526 MiB, k3d 398 MiB | same |
| GitHub Actions job, Helm chart e2e (setup, install, test, upgrade) | **21 s**, cluster ready in 2.6 s | kind (helm/kind-action): 79 s, 44.7 s | [docs/ci.md](ci.md#numbers) |
| Metrics, logs and alerts, added memory | **+1.8 MiB** | kube-prometheus-stack + Loki: 1,371 MiB | [bench/RESULTS.md](../bench/RESULTS.md#observability-built-in-vs-kube-prometheus-stack--loki-2026-10-20-x86_64kvm) |
| Compatibility corpus (kubectl, CRDs, workloads, networking, 47 Helm charts) | **119/119** | k3s: 119/119 | [compat/RESULTS.md](../compat/RESULTS.md#without-docker-2026-10-19-119-cases) |
| Kubernetes v1.33 conformance (419 specs) | **413/419** | k3s: 416/419 | [compat/CONFORMANCE.md](../compat/CONFORMANCE.md) |

On macOS a dev cluster runs in a Linux VM, and Activity Monitor shows the
whole VM: 238 MiB fresh, ~380 MiB after pulling and running 5 nginx pods
(the cluster itself is 30 and 79 MiB of that). macOS keeps charging the VM
for every page its guest has touched until the VM stops
([docs/dev.md](dev.md#memory-on-macos)).

The 6 conformance failures: 5 are macOS-only (TCP DNS on port 53, a
hostPort binding) and pass on Linux; 1 was a watch fixed since. A 3-server
HA control plane stays linearizable under leader kills and partitions
([bench/HA.md](../bench/HA.md)); a 3-node cluster ran 21 h under churn with no
leak ([bench/SOAK.md](../bench/SOAK.md)).
