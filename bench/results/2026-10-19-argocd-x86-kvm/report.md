Argo CD chart 10.10.5 (defaults), 2 vCPU / 2048 MiB amd64 guests on AMD Ryzen 7 5800X 8-Core Processor, litek8s `76dc2cb`; runs: run1, run2, run3

### Time

| | k3s minimal | litek8s (crun) |
|---|---|---|
| `helm install` → all Argo CD pods Ready, images pulled | 27.1 s (27.0–72.5, n=3) | 19.9 s (19.5–20.0, n=3) |
| … again after `helm uninstall`, images cached | 20.0 s (19.1–44.7, n=9) | 13.4 s (13.3–13.6, n=9) |
| … of which `helm install` itself (pre-install hook Job) | 5.8 s (5.7–16.1, n=9) | 2.1 s (2.0–2.1, n=9) |
| Application `guestbook` → Synced + Healthy, first (clone, image pull) | 15.7 s (15.6–16.6, n=3) | 18.1 s (18.0–19.1, n=3) |
| … recreated (cached) | 1.3 s (1.2–1.3, n=9) | 1.0 s (1.0–1.0, n=9) |
| Application `helm-guestbook` → Synced + Healthy, first | 2.4 s (2.4–2.4, n=3) | 1.2 s (1.2–1.2, n=3) |
| … recreated | 1.8 s (1.5–1.8, n=9) | 1.0 s (1.0–1.0, n=9) |
| helm parameter replicaCount 1 → 2: re-synced, 2 pods Ready | 1.9 s (1.6–2.5, n=9) | 1.0 s (1.0–1.0, n=9) |
| replicaCount 2 → 1: re-synced | 0.8 s (0.7–1.1, n=9) | 0.7 s (0.7–1.0, n=9) |
| managed Deployment deleted → recreated by self-heal | 0.4 s (0.4–7.2, n=9) | 0.3 s (0.3–0.4, n=9) |
| … → Available again, app Synced + Healthy | 1.1 s (0.8–7.9, n=9) | 0.4 s (0.4–0.4, n=9) |

### Memory, PSS MiB (`run1`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| Orchestrator, no Argo CD (control plane + runtime + addons) | 586 | 19 |
| Orchestrator, Argo CD + 1 app idle | 882 | 90 |
| … control plane (k3s-server / litek8s apiserver) | 725 | 76 |
| … container runtime (containerd, shims, pause / crun shims) | 136 | 13 |
| … addon pods (CoreDNS) | 21 | 0 |
| Argo CD pods, idle with 1 app | 377 | 359 |
| Orchestrator, peak while syncing (5 s samples) | 937 | 130 |
| Argo CD pods, peak while syncing | 448 | 381 |
| Guest MemAvailable, no Argo CD | 1274 | 1627 |
| Guest MemAvailable, Argo CD + 1 app idle | 578 | 1337 |
| Guest MemAvailable, lowest while syncing | 499 | 1283 |
| Control plane after the 5 min idle window | 714 | 75 |
| Argo CD application/applicationset controller | 164 | 119 |
| Argo CD dex | 63 | 93 |
| Argo CD notifications controller | 39 | 36 |
| Argo CD redis | 22 | 23 |
| Argo CD repo server | 47 | 46 |
| Argo CD server | 42 | 42 |

### Memory, PSS MiB (`run2`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| Orchestrator, no Argo CD (control plane + runtime + addons) | 572 | 19 |
| Orchestrator, Argo CD + 1 app idle | 928 | 90 |
| … control plane (k3s-server / litek8s apiserver) | 765 | 76 |
| … container runtime (containerd, shims, pause / crun shims) | 136 | 13 |
| … addon pods (CoreDNS) | 28 | 0 |
| Argo CD pods, idle with 1 app | 342 | 363 |
| Orchestrator, peak while syncing (5 s samples) | 1002 | 130 |
| Argo CD pods, peak while syncing | 406 | 383 |
| Guest MemAvailable, no Argo CD | 1309 | 1633 |
| Guest MemAvailable, Argo CD + 1 app idle | 536 | 1333 |
| Guest MemAvailable, lowest while syncing | 488 | 1270 |
| Control plane after the 5 min idle window | 759 | 75 |
| Argo CD application/applicationset controller | 168 | 121 |
| Argo CD dex | 30 | 93 |
| Argo CD notifications controller | 37 | 36 |
| Argo CD redis | 21 | 23 |
| Argo CD repo server | 46 | 47 |
| Argo CD server | 41 | 44 |

### Memory, PSS MiB (`run3`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| Orchestrator, no Argo CD (control plane + runtime + addons) | 586 | 19 |
| Orchestrator, Argo CD + 1 app idle | 900 | 90 |
| … control plane (k3s-server / litek8s apiserver) | 736 | 77 |
| … container runtime (containerd, shims, pause / crun shims) | 137 | 13 |
| … addon pods (CoreDNS) | 26 | 0 |
| Argo CD pods, idle with 1 app | 329 | 354 |
| Orchestrator, peak while syncing (5 s samples) | 945 | 132 |
| Argo CD pods, peak while syncing | 416 | 376 |
| Guest MemAvailable, no Argo CD | 1280 | 1633 |
| Guest MemAvailable, Argo CD + 1 app idle | 571 | 1341 |
| Guest MemAvailable, lowest while syncing | 524 | 1269 |
| Control plane after the 5 min idle window | 741 | 75 |
| Argo CD application/applicationset controller | 158 | 115 |
| Argo CD dex | 28 | 92 |
| Argo CD notifications controller | 38 | 36 |
| Argo CD redis | 18 | 23 |
| Argo CD repo server | 45 | 47 |
| Argo CD server | 42 | 42 |

### Idle CPU with Argo CD and 1 synced app, % of one vCPU (`run1`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| whole guest busy | 3.08 | 0.41 |
| control-plane | 1.95 | 0.05 |
| runtime | 1.02 | 0.00 |
| addons | 0.08 | 0.00 |
| argocd | 0.37 | 0.39 |
| workload | 0.00 | 0.01 |
| os/other | 0.00 | 0.00 |

### Idle CPU with Argo CD and 1 synced app, % of one vCPU (`run2`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| whole guest busy | 2.98 | 0.43 |
| control-plane | 1.91 | 0.06 |
| runtime | 1.03 | 0.00 |
| addons | 0.07 | 0.00 |
| argocd | 0.36 | 0.45 |
| workload | 0.00 | 0.00 |
| os/other | 0.00 | 0.00 |

### Idle CPU with Argo CD and 1 synced app, % of one vCPU (`run3`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| whole guest busy | 2.93 | 0.46 |
| control-plane | 1.92 | 0.06 |
| runtime | 1.05 | 0.00 |
| addons | 0.07 | 0.00 |
| argocd | 0.35 | 0.46 |
| workload | 0.00 | 0.00 |
| os/other | 0.00 | 0.00 |

### Host load while each phase ran (all host CPUs, the VM included)

| run | system | host busy, whole phase | host busy, idle window | load avg (1 min) start → end |
|---|---|---|---|---|
| run1 | k3s minimal | 3.3 % | 1.1 % | 0.46 → 0.02 |
| run1 | litek8s (crun) | 1.9 % | 0.6 % | 0.02 → 0.13 |
| run2 | k3s minimal | 3.0 % | 1.0 % | 0.04 → 0.00 |
| run2 | litek8s (crun) | 1.9 % | 0.5 % | 0.12 → 0.04 |
| run3 | k3s minimal | 3.1 % | 1.1 % | 0.16 → 0.77 |
| run3 | litek8s (crun) | 1.9 % | 0.5 % | 0.77 → 0.11 |

### API requests seen by the API server (`run1`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| no Argo CD (60 s) | 0.77/s | 0.02/s |
| Argo CD + 1 app idle | 1.22/s | 0.10/s |
| during the GitOps steps (all clients) | 13.44/s | 7.67/s |
| … of which the benchmark driver: GETs + other kubectl runs | 202 + 24 | 175 + 24 |
| during the GitOps steps without the driver's GETs | 12.16/s | 6.35/s |
| open watches, no Argo CD | 162 | 0 |
| open watches, Argo CD + 1 app | 265 | 79 |

k3s minimal, idle window (301 s, 368 requests): GET /openapi/v3 60, PUT leases 58, GET endpoints 30, GET endpointslices 30, GET /api 20, GET /apis 20, WATCH configmaps 18, WATCH secrets 18, WATCH nodes 8, PATCH nodes/status 6, WATCH namespaces 5, WATCH applications 5

k3s minimal, sync window (157 s, 2107 requests): GET applications 279, POST events 256, PATCH applications 166, GET deployments 124, GET pods 115, PUT deployments/status 107, GET namespaces 79, PATCH pods/status 75, PUT replicasets/status 72, GET /openapi/v3 68, GET services 64, GET replicasets 43

litek8s (crun), idle window (301 s, 29 requests): WATCH secrets 13, WATCH configmaps 7, WATCH applications 4, WATCH appprojects 2, GET (discovery, openapi, ...) 1, PATCH applications 1, WATCH applicationsets 1

litek8s (crun), sync window (133 s, 1018 requests): GET applications 215, GET (discovery, openapi, ...) 188, PATCH applications 126, POST events 122, GET namespaces 70, GET deployments 66, GET services 28, PUT applications 18, POST deployments 11, DELETE deployments 10, LIST pods 8, LIST deployments 8

### API requests seen by the API server (`run2`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| no Argo CD (60 s) | 0.75/s | 0.02/s |
| Argo CD + 1 app idle | 1.25/s | 0.09/s |
| during the GitOps steps (all clients) | 12.77/s | 8.01/s |
| … of which the benchmark driver: GETs + other kubectl runs | 226 + 24 | 176 + 24 |
| during the GitOps steps without the driver's GETs | 11.38/s | 6.69/s |
| open watches, no Argo CD | 162 | 0 |
| open watches, Argo CD + 1 app | 265 | 79 |

k3s minimal, idle window (301 s, 376 requests): GET /openapi/v3 60, PUT leases 59, GET endpoints 30, GET endpointslices 30, GET /api 20, GET /apis 20, WATCH configmaps 20, WATCH secrets 17, WATCH nodes 6, PATCH nodes/status 6, WATCH endpointslices 6, WATCH namespaces 3

k3s minimal, sync window (163 s, 2079 requests): GET applications 276, POST events 246, PATCH applications 155, GET deployments 136, GET pods 111, PUT deployments/status 108, GET namespaces 75, PATCH pods/status 75, PUT replicasets/status 72, GET /openapi/v3 65, GET services 60, GET replicasets 43

litek8s (crun), idle window (301 s, 27 requests): WATCH secrets 12, WATCH configmaps 5, WATCH applications 3, WATCH appprojects 3, PATCH applications 2, GET (discovery, openapi, ...) 1, WATCH applicationsets 1

litek8s (crun), sync window (133 s, 1067 requests): GET applications 221, GET (discovery, openapi, ...) 205, PATCH applications 134, POST events 126, GET namespaces 73, GET deployments 72, GET services 30, PUT applications 21, POST deployments 11, DELETE deployments 10, LIST pods 8, LIST deployments 8

### API requests seen by the API server (`run3`)

| | k3s minimal | litek8s (crun) |
|---|---|---|
| no Argo CD (60 s) | 0.78/s | 0.02/s |
| Argo CD + 1 app idle | 1.22/s | 0.10/s |
| during the GitOps steps (all clients) | 12.85/s | 7.66/s |
| … of which the benchmark driver: GETs + other kubectl runs | 223 + 24 | 179 + 24 |
| during the GitOps steps without the driver's GETs | 11.48/s | 6.32/s |
| open watches, no Argo CD | 162 | 0 |
| open watches, Argo CD + 1 app | 265 | 79 |

k3s minimal, idle window (301 s, 367 requests): GET /openapi/v3 60, PUT leases 58, GET endpoints 30, GET endpointslices 30, GET /api 20, GET /apis 20, WATCH secrets 20, WATCH configmaps 17, WATCH nodes 7, PATCH nodes/status 6, WATCH pods 5, WATCH applications 5

k3s minimal, sync window (163 s, 2089 requests): GET applications 279, POST events 249, PATCH applications 162, GET deployments 141, GET pods 112, PUT deployments/status 108, GET namespaces 76, PATCH pods/status 76, PUT replicasets/status 72, GET /openapi/v3 66, GET services 58, GET replicasets 40

litek8s (crun), idle window (301 s, 31 requests): WATCH secrets 13, WATCH configmaps 7, WATCH applications 3, WATCH appprojects 3, PATCH applications 2, WATCH applicationsets 2, GET (discovery, openapi, ...) 1

litek8s (crun), sync window (134 s, 1027 requests): GET applications 222, GET (discovery, openapi, ...) 190, PATCH applications 125, POST events 121, GET namespaces 70, GET deployments 66, GET services 30, PUT applications 18, POST deployments 11, DELETE deployments 10, LIST pods 8, LIST deployments 8
