# Numbers

Measured against k3s, kind and k3d with the scripts in this repository; the
linked files have the setup, raw data and caveats.

| | litek8s | reference | source |
|---|---|---|---|
| Whole orchestrator, 5 nginx pods (PSS, 1 GiB arm64 VM) | **24.6 MiB** | k3s minimal: 603 MiB | [bench/RESULTS.md](../bench/RESULTS.md#the-kubernetes-path-on-crun-without-docker-2026-10-17) |
| Argo CD + one synced app, orchestrator memory | **90 MiB** | k3s minimal: 900 MiB | [bench/RESULTS.md](../bench/RESULTS.md#argo-cd-on-k3s-and-on-litek8s-2026-10-19-x86_64kvm) |
| `dev up` → node Ready, Linux / macOS | **0.17 s / 1.28 s** | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s | [bench/RESULTS.md](../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18) |
| Dev cluster idle memory, Linux | **47 MiB** | kind 526 MiB, k3d 398 MiB | same |
| Metrics, logs and alerts, added memory | **+1.8 MiB** | kube-prometheus-stack + Loki: 1,371 MiB | [bench/RESULTS.md](../bench/RESULTS.md#observability-built-in-vs-kube-prometheus-stack--loki-2026-10-20-x86_64kvm) |
| Compatibility corpus (kubectl, CRDs, workloads, networking, 47 Helm charts) | **119/119** | k3s: 119/119 | [compat/RESULTS.md](../compat/RESULTS.md#without-docker-2026-10-19-119-cases) |
| Kubernetes v1.33 conformance (419 specs) | **413/419** | k3s: 416/419 | [compat/CONFORMANCE.md](../compat/CONFORMANCE.md) |

The 6 conformance failures: 5 are macOS-only (TCP DNS on port 53, a
hostPort binding) and pass on Linux; 1 was a watch fixed since. A 3-server
HA control plane stays linearizable under leader kills and partitions
([bench/HA.md](../bench/HA.md)); a 3-node cluster ran 21 h under churn with no
leak ([bench/SOAK.md](../bench/SOAK.md)).
