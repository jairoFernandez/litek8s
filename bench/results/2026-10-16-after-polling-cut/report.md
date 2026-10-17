Run `2026-10-16-after-polling-cut` — 2 vCPU / 1024 MiB guest on Apple M3 Max, idle window 300s, litek8s `cfb4222`.

### Idle memory (PSS MiB; RSS in parentheses)

| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |
|---|---|---|---|---|---|---|---|
| litek8s | 0 | 11.0 (12.7) | 116.0 (119.5) | – | **127.0** (132.3) | – | 77.8 |
| litek8s | 5 | 13.5 (15.2) | 169.1 (200.3) | – | **182.6** (215.5) | 17.6 | 109.6 |

### Idle CPU (% of one vCPU, averaged over the window)

| system | workloads | control plane | container runtime | addon pods | whole guest busy |
|---|---|---|---|---|---|
| litek8s | 0 | 0.41 | 0.33 | 0.00 | **0.81** |
| litek8s | 5 | 0.41 | 2.14 | 0.00 | **2.55** |

### Apply → first ready container (seconds, image pre-pulled)

| system | runs | median | min | max | of which CLI apply |
|---|---|---|---|---|---|
| litek8s | 5 | **0.21** | 0.18 | 0.35 | 0.15 |

### On-disk footprint (MiB)

| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |
|---|---|---|---|
| litek8s | 22.7 | docker.io 29.1.3-0ubuntu3~24.04.2 + deps: 254.2 (dpkg Installed-Size) | **284.9** |

### Control-plane writes during the 5-workload idle window

| system | write() KiB/min by control plane | state on disk (bytes) |
|---|---|---|
| litek8s | 0 | 23634 |

### litek8s remote-node RPC cost (control-plane VM → node VM over SSH ControlMaster)

| path | mean ms | p50 | p95 | caller CPU ms/RPC | node CPU ms/RPC |
|---|---|---|---|---|---|
| ssh mux, `true` (transport floor) | 5.59 | 5.41 | 6.71 | 4.4 | 2.4 |
| ssh mux + `litek8s agent` exec, Ping | 7.80 | 7.57 | 9.33 | 4.4 | 4.6 |
| ssh mux + `litek8s agent` exec, ListContainersForDeployment | 7.89 | 7.58 | 9.60 | 4.6 | 4.6 |
| resident agent socket, Ping (no ssh/exec) | 0.26 | 0.23 | 0.37 | – | – |
| resident agent socket, ListContainersForDeployment | 0.25 | 0.23 | 0.30 | – | – |

### RPC census: 5 nginx deployments on one SSH node, steady state, 60s

125 RPCs = **2.1 RPC/s** (each one an `ssh` fork on the control plane and an sshd session + `litek8s agent` exec on the node). Reconcile passes observed: 29 (≈ every 2.1s), **1.9 RPCs per reconcile pass**.

| request | count | per second | loop |
|---|---|---|---|
| Ping | 60 | 1.00 | cluster snapshot |
| ListManagedContainers | 29 | 0.48 | reconcile |
| ProbeContainerTcp | 26 | 0.43 | reconcile |
| HostResources | 10 | 0.17 | cluster snapshot |

### Steady-state CPU with those 5 deployments on the remote node (% of one vCPU)

| where | top consumers | whole guest busy |
|---|---|---|
| node VM | sshd-session + `litek8s agent` execs 1.26, dockerd 0.81, containerd 0.80, os/other 0.29, litek8s-agent 0.11 | **3.41** |
| control-plane VM | `ssh` clients forked by litek8s serve 1.06, litek8s-serve 0.36, os/other 0.24, containerd 0.14, dockerd 0.07 | **1.89** |
| (same 5 deployments on litek8s's local node, for reference) | | 2.55 |

