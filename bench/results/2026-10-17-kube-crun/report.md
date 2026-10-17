Run `2026-10-17-kube-crun` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `b5bb4b2`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 15.5 (17.0) | 154.2 (157.7) | – | **169.7** (174.7) | – | 109.2 |
| litek8s apiserver + agent | 5 | 18.3 (19.8) | 191.2 (265.1) | – | **209.5** (284.9) | 17.6 | 132.1 |
| litek8s apiserver + agent, crun | 0 | 14.0 (15.5) | – | – | **14.0** (15.5) | – | 26.2 |
| litek8s apiserver + agent, crun | 5 | 17.2 (20.4) | 7.4 (18.3) | – | **24.6** (38.7) | 17.6 | 79.6 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 0 | 0.00 | 0.14 | 0.00 | **0.20** |
| litek8s apiserver + agent | 5 | 0.13 | 0.28 | 0.00 | **0.48** |
| litek8s apiserver + agent, crun | 0 | 0.00 | 0.00 | 0.00 | **0.08** |
| litek8s apiserver + agent, crun | 5 | 0.12 | 0.00 | 0.00 | **0.21** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s apiserver + agent | 5 | **0.22** | 0.20 | 0.24 | 0.01 |
| litek8s apiserver + agent, crun | 5 | **0.08** | 0.08 | 0.08 | 0.01 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s apiserver + agent | 32.5 | docker.io 29.1.3-0ubuntu3~24.04.2 + deps: 254.2 (dpkg Installed-Size) | **325.5** |
| litek8s apiserver + agent, crun | 32.5 | crun 1.14.1-1 + deps: 2.1 (dpkg Installed-Size) | **41.5** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s apiserver + agent | 0 | 271184 |
| litek8s apiserver + agent, crun | 0 | 380964995 |

