Observability on k3s vs built into litek8s, measured window 30 min after a warm-up, same workload (bench/obs/workload.yaml).

### Memory, PSS MiB (end of the window)

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Orchestrator, workload only (no observability) | 667 | 0 |
| Orchestrator, with observability | 1,053 | 0 |
| Observability stack's pods | 1,020 | 0 |
| **Observability's cost** (orchestrator growth + its pods) | **1,406** | **0** |
| Guest memory used (MemTotal − MemAvailable), before → after | 840 → 1,776 | 0 → 0 |

### CPU, % of one vCPU (average over the window)

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Control plane, workload only | 1.79 | 0.00 |
| Control plane, with observability | 3.03 | 0.00 |
| Observability stack's pods | 1.70 | 0.00 |
| Runtime (containerd/shims, crun shims) | 2.19 | 0.00 |
| **Observability's cost** | **4.10** | **0.00** |
| Whole guest busy, % of all vCPUs | 4.37 | - |

### Disk

| | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| Installed (images, binaries) | 2.0 GiB | - |
| Series | 70,818 | - |
| Samples ingested / s | 2,424.5 | - |
| Metric store growth over the window (all files) | 21.4 MiB | 0 B |
| … compressed samples only (Prometheus: chunks_head; litek8s: blocks) | 128.0 KiB | 0 B |
| Bytes per sample (compressed) | 0.03 | - |
| Log store growth over the window (Loki after a flush, WAL included; litek8s: its archive) | 1.5 MiB | 0 B |
| … compressed chunks only (Loki) | 1.2 MiB | – |
| Container log files (CRI, both systems keep them) | 8.7 MiB | - |
| Metrics per day of retention, compressed | 6.0 MiB | 0 B |
| Logs per day of retention (Loki: chunks) | 56.6 MiB | 0 B |

### Queries, median ms (p90), 20 runs each

| query | k3s minimal + kube-prometheus-stack + Loki + Alloy | litek8s (built in) |
|---|---|---|
| cpu by namespace (instant) | 0.4 (0.5), 3 series | - |
| top 5 pods by memory (instant) | 0.4 (0.4), 5 series | - |
| pods by phase (instant) | 0.5 (0.5), 5 series | - |
| restarts in 1h (instant) | 0.4 (0.5), 14 series | - |
| node memory available (instant) | 0.5 (0.5), 1 series | - |
| node cpu busy (instant) | 0.4 (0.5), 1 series | - |
| cpu vs requests (instant) | 0.4 (0.5), 5 series | - |
| cpu by namespace (1 h range, 15 s step) | 0.9 (1.0), 3 series | - |
| top 5 pods by memory (1 h range, 15 s step) | 0.7 (0.8), 9 series | - |
| pods by phase (1 h range, 15 s step) | 0.8 (1.0), 5 series | - |
| restarts in 1h (1 h range, 15 s step) | 1.0 (1.7), 14 series | - |
| node memory available (1 h range, 15 s step) | 0.4 (0.5), 1 series | - |
| node cpu busy (1 h range, 15 s step) | 0.7 (0.7), 1 series | - |
| cpu vs requests (1 h range, 15 s step) | 0.8 (0.8), 5 series | - |
| logs: errors of app=loggen, 1h | 29.3 (39.0), 1000 lines | - |
| logs: 'timeout' in every pod, 15m | 15.2 (20.2), 1000 lines | - |

- k3s minimal + kube-prometheus-stack + Loki + Alloy: v1.36.5+k3s1, k3s version v1.36.5+k3s1 (3dd98cc5); 14 pods, 0 restarts
  charts: alloy-1.14.0 (v1.21.0), kube-prometheus-stack-92.10.1 (v0.94.1), loki-7.3.0 (3.6.12)
