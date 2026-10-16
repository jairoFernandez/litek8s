Run `2026-10-16-t4g-micro-shape` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `78fc083`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s | 0 | 11.4 (13.1) | 116.3 (119.8) | – | **127.6** (132.9) | – | 55.6 |
| litek8s | 5 | 14.1 (15.8) | 174.0 (205.7) | – | **188.2** (221.5) | 17.5 | 110.2 |
| k3s (default) | 0 | 523.1 (527.8) | 87.9 (101.4) | 81.6 (81.6) | **692.6** (710.8) | – | 638.8 |
| k3s (default) | 5 | 538.9 (539.6) | 102.5 (105.7) | 56.4 (56.5) | **697.8** (701.8) | 12.9 | 685.5 |
| k3s (minimal) | 0 | 400.3 (456.9) | 116.5 (173.1) | 55.3 (55.3) | **572.2** (685.4) | – | 357.5 |
| k3s (minimal) | 5 | 401.9 (458.8) | 144.1 (258.8) | 56.7 (56.7) | **602.6** (774.4) | 17.4 | 420.1 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s | 0 | 0.47 | 0.34 | 0.00 | **0.89** |
| litek8s | 5 | 0.59 | 5.27 | 0.00 | **5.81** |
| k3s (default) | 0 | 3.98 | 1.11 | 0.54 | **6.26** |
| k3s (default) | 5 | 8.75 | 3.10 | 2.35 | **20.42** |
| k3s (minimal) | 0 | 3.18 | 0.55 | 0.19 | **4.18** |
| k3s (minimal) | 5 | 3.34 | 1.17 | 0.17 | **5.15** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s | 10 | **0.16** | 0.16 | 0.39 | 0.14 |
| k3s (default) | 10 | **1.75** | 1.65 | 3.11 | 0.39 |
| k3s (minimal) | 10 | **0.79** | 0.76 | 1.53 | 0.10 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s | 22.6 | docker.io 29.1.3-0ubuntu3~24.04.2 + deps: 254.2 (dpkg Installed-Size) | **284.8** |
| k3s (default) | 68.1 | extracted data dir 236.4 + addon images 741.8 (k3s version v1.36.5+k3s1 (3dd98cc5)) | **1064.1** |
| k3s (minimal) | 68.1 | extracted data dir 236.4 + addon images 95.2 (k3s version v1.36.5+k3s1 (3dd98cc5)) | **406.1** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s | 0 | 29634 |
| k3s (default) | 2652 | 19140792 |
| k3s (minimal) | 1641 | 9051992 |

### litek8s remote-node RPC cost (control-plane VM → node VM over SSH ControlMaster)

| path | mean ms | p50 | p95 | caller CPU ms/RPC | node CPU ms/RPC |
|---|---|---|---|---|---|
| ssh mux, `true` (transport floor) | 4.99 | 4.91 | 5.52 | 4.4 | 2.2 |
| ssh mux + `litek8s agent` exec, Ping | 7.22 | 7.06 | 8.04 | 4.4 | 4.8 |
| ssh mux + `litek8s agent` exec, ListContainersForDeployment | 7.36 | 7.25 | 8.09 | 4.6 | 5.0 |
| resident agent socket, Ping (no ssh/exec) | 0.23 | 0.21 | 0.28 | – | – |
| resident agent socket, ListContainersForDeployment | 0.23 | 0.20 | 0.32 | – | – |

### RPC census: 5 nginx deployments on one SSH node, steady state, 60s

894 RPCs = **14.9 RPC/s** (each one an `ssh` fork on the control plane and an sshd session + `litek8s agent` exec on the node). Reconcile passes observed: 30 (≈ every 2.0s), **11.2 RPCs per reconcile pass**.

| request | count | per second | loop |
|---|---|---|---|
| StatsSnapshot | 450 | 7.50 | cluster snapshot |
| ListContainersForDeployment | 155 | 2.58 | reconcile |
| ProbeContainerTcp | 151 | 2.52 | reconcile |
| Ping | 97 | 1.62 | cluster snapshot |
| ListManagedContainers | 30 | 0.50 | reconcile |
| HostResources | 11 | 0.18 | cluster snapshot |

### Steady-state CPU with those 5 deployments on the remote node (% of one vCPU)

| where | top consumers | whole guest busy |
|---|---|---|
| node VM | sshd-session + `litek8s agent` execs 6.32, dockerd 2.58, containerd 2.08, os/other 1.01, containerd-shim 0.53 | **13.33** |
| control-plane VM | `ssh` clients forked by litek8s serve 5.80, os/other 1.13, litek8s-serve 0.63, containerd 0.14, dockerd 0.08 | **7.78** |
| (same 5 deployments on litek8s's local node, for reference) | | 5.81 |

