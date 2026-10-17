Run `2026-10-17-kube-crun-merged` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `18d0909`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s apiserver + agent, crun | 0 | 14.0 (15.6) | – | – | **14.0** (15.6) | – | 13.0 |
| litek8s apiserver + agent, crun | 5 | 17.4 (20.5) | 7.2 (17.4) | – | **24.7** (37.8) | 17.6 | 77.2 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s apiserver + agent, crun | 0 | 0.00 | 0.00 | 0.00 | **0.08** |
| litek8s apiserver + agent, crun | 5 | 0.15 | 0.00 | 0.00 | **0.23** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s apiserver + agent, crun | 5 | **0.06** | 0.06 | 0.08 | 0.01 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s apiserver + agent, crun | 33.1 | crun 1.14.1-1 + deps: 2.1 (dpkg Installed-Size) | **42.2** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s apiserver + agent, crun | 1 | 295759 |

