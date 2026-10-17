Run `2026-10-17-kube` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `bd117b5`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 15.0 (16.5) | 161.6 (165.2) | – | **176.6** (181.6) | – | 104.2 |
| litek8s apiserver + agent | 5 | 17.7 (19.2) | 196.9 (277.0) | – | **214.6** (296.2) | 17.6 | 152.9 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 0.00 | 0.10 | 0.00 | **0.17** |
| litek8s apiserver + agent | 5 | 0.11 | 0.32 | 0.00 | **0.49** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 5 | **2.19** | 2.16 | 2.24 | 0.01 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s apiserver + agent | 31.1 | docker.io 29.1.3-0ubuntu3~24.04.2 + deps: 254.2 (dpkg Installed-Size) | **324.3** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s apiserver + agent | 0 | 271058 |

