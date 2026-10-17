Run `2026-10-17-kube-after` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `bd117b5`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 14.5 (16.0) | 151.5 (155.1) | – | **166.1** (171.1) | – | 101.4 |
| litek8s apiserver + agent | 5 | 17.5 (18.9) | 191.3 (265.1) | – | **208.8** (284.0) | 17.6 | 139.1 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 0.00 | 0.11 | 0.00 | **0.18** |
| litek8s apiserver + agent | 5 | 0.08 | 0.28 | 0.00 | **0.46** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 5 | **0.20** | 0.20 | 0.31 | 0.01 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s apiserver + agent | 31.1 | docker.io 29.1.3-0ubuntu3~24.04.2 + deps: 254.2 (dpkg Installed-Size) | **324.1** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s apiserver + agent | 0 | 271058 |

