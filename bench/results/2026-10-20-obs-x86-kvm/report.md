Observability on k3s vs built into litek8s, measured window 60 min after a warm-up, same workload (bench/obs/workload.yaml).

### Memory, PSS MiB (end of the window)

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Orchestrator, workload only (no observability) | 681 | 40 |
| Orchestrator, with observability | 995 | 37 |
| Observability stack's pods | 1,057 | 0 |
| **Observability's cost** (orchestrator growth + its pods) | **1,371** | **-3** |
| Guest memory used (MemTotal − MemAvailable), before → after | 842 → 1,739 | 433 → 426 |

### CPU, % of one vCPU (average over the window)

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Control plane, workload only | 1.86 | 0.01 |
| Control plane, with observability | 3.13 | 0.04 |
| Observability stack's pods | 1.79 | 0.00 |
| Runtime (containerd/shims, crun shims) | 2.21 | 0.05 |
| **Observability's cost** | **4.20** | **0.04** |
| Whole guest busy, % of all vCPUs | 4.35 | 1.16 |

### Disk

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Installed (images, binaries) | 2.1 GiB | 2.0 MiB |
| Series | 69,672 | 342 |
| Samples ingested / s | 2,349.0 | 34.1 |
| Metric store growth over the window (all files) | 50.1 MiB | 232.0 KiB |
| … compressed samples only (Prometheus: chunks_head; litek8s: blocks) | 0 B | 192.0 KiB |
| Bytes per sample (compressed) | 0.00 | 1.60 |
| Log store growth over the window (Loki after a flush, WAL included; litek8s: its archive) | 3.8 MiB | 0 B |
| … compressed chunks only (Loki) | 2.2 MiB | – |
| Container log files (CRI, both systems keep them) | 18.9 MiB | 18.1 MiB |
| Metrics per day of retention, compressed | 0 B | 4.5 MiB |
| Logs per day of retention (Loki: chunks) | 51.9 MiB | 0 B |

### Queries, median ms (p90), 20 runs each

| query | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| cpu by namespace (instant) | 0.4 (1.0), 3 series | 0.3 (0.3), 1 series |
| top 5 pods by memory (instant) | 0.4 (0.4), 5 series | 0.3 (0.3), 5 series |
| pods by phase (instant) | 0.4 (0.4), 5 series | 0.3 (0.4), 5 series |
| restarts in 1h (instant) | 0.5 (0.5), 14 series | 0.4 (0.4), 5 series |
| node memory available (instant) | 0.3 (0.4), 1 series | 0.2 (0.3), 1 series |
| node cpu busy (instant) | 0.5 (0.9), 1 series | 0.2 (0.3), 1 series |
| cpu vs requests (instant) | 0.4 (0.6), 5 series | 0.3 (0.3), 5 series |
| cpu by namespace (1 h range, 15 s step) | 1.2 (1.3), 3 series | 1.4 (1.4), 1 series |
| top 5 pods by memory (1 h range, 15 s step) | 0.9 (1.0), 5 series | 2.0 (2.2), 5 series |
| pods by phase (1 h range, 15 s step) | 1.0 (1.2), 5 series | 4.3 (4.4), 5 series |
| restarts in 1h (1 h range, 15 s step) | 1.5 (1.6), 14 series | 3.3 (3.5), 5 series |
| node memory available (1 h range, 15 s step) | 0.4 (0.8), 1 series | 0.8 (0.8), 1 series |
| node cpu busy (1 h range, 15 s step) | 0.7 (0.8), 1 series | 1.3 (1.4), 1 series |
| cpu vs requests (1 h range, 15 s step) | 1.0 (1.3), 5 series | 3.5 (3.6), 5 series |
| logs: errors of app=loggen, 1h | 49.2 (61.1), 1000 lines | 33.3 (37.0), 1000 lines |
| logs: 'timeout' in every pod, 15m | 19.8 (23.4), 1000 lines | 31.8 (36.2), 1000 lines |

- k3s minimal + kube-prometheus-stack + Loki + Alloy: v1.36.5+k3s1, k3s version v1.36.5+k3s1 (3dd98cc5); 14 pods, 0 restarts
  charts: alloy-1.14.0 (v1.21.0), kube-prometheus-stack-92.10.1 (v0.94.1), loki-7.3.0 (3.6.12)
- litek8s (built in): v1.33.0+litek8s, crun version 1.14.1; 5 pods, 0 restarts
  charts: 
