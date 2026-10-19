1264 samples over 21.1 h

| series | first | last | min | max | slope/h (2nd half) |
|---|---:|---:|---:|---:|---:|
| API delete mean ms | 0.25 | 0.10 | 0.01 | 7.37 | +0.010 |
| API get mean ms | 0.20 | 0.06 | 0.03 | 5043.66 | +0.016 |
| API list mean ms | 0.23 | 0.26 | 0.22 | 3.50 | +0.039 |
| API patch mean ms | 0.16 | 0.12 | 0.11 | 23.15 | +0.016 |
| GET /api/v1/namespaces/soak/configmaps ms | 0.45 | 0.18 | 0.17 | 8.08 | +0.031 |
| GET /api/v1/namespaces/soak/pods ms | 0.85 | 0.71 | 0.68 | 22.90 | +0.164 |
| GET /api/v1/nodes ms | 0.42 | 0.36 | 0.31 | 10.05 | +0.048 |
| GET /apis/apps/v1/namespaces/soak/deployments/web ms | 0.47 | 0.25 | 0.17 | 6.11 | +0.049 |
| container restarts | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| litek8s_agent_connections | 3.00 | 2.00 | 2.00 | 3.00 | +0.000 |
| litek8s_agent_streams | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| litek8s_apiserver_watches | 4.00 | 1.00 | 0.00 | 4.00 | +0.006 |
| litek8s_process_resident_MiB | 27.39 | 34.07 | 27.39 | 34.65 | +0.056 |
| litek8s_store_compactions_total | 0.00 | 3.00 | 0.00 | 3.00 | +0.165 |
| litek8s_store_feed_receivers | 8.00 | 4.00 | 3.00 | 8.00 | +0.006 |
| litek8s_store_history_MiB | 0.24 | 1.48 | 0.24 | 1.86 | +0.012 |
| litek8s_store_objects | 65.00 | 83.00 | 65.00 | 83.00 | +0.004 |
| litek8s_store_wal_MiB | 0.29 | 22.61 | 0.14 | 28.41 | +0.469 |
| litek8s_store_wal_records | 178.00 | 15976.00 | 103.00 | 20079.00 | +323.316 |
| lk1 MemAvailable MiB | 715.00 | 661.27 | 659.27 | 715.00 | -0.385 |
| lk1 apiserver CPU % | 0.08 | 0.07 | 0.00 | 2.67 | +0.028 |
| lk1 apiserver PSS MiB | 24.44 | 31.07 | 24.44 | 31.60 | +0.054 |
| lk1 apiserver count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk1 apiserver fds | 20.00 | 21.00 | 19.00 | 24.00 | +0.057 |
| lk1 apiserver threads | 4.00 | 3.00 | 3.00 | 4.00 | -0.025 |
| lk1 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.07 | +0.000 |
| lk1 crun-shim PSS MiB | 6.12 | 7.38 | 6.11 | 11.04 | +0.069 |
| lk1 crun-shim count | 4.00 | 5.00 | 4.00 | 8.00 | +0.056 |
| lk1 crun-shim fds | 32.00 | 40.00 | 32.00 | 64.00 | +0.450 |
| lk1 crun-shim threads | 4.00 | 5.00 | 4.00 | 8.00 | +0.056 |
| lk2 MemAvailable MiB | 705.36 | 719.75 | 687.37 | 752.75 | +0.862 |
| lk2 agent CPU % | 0.12 | 0.03 | 0.00 | 1.00 | +0.010 |
| lk2 agent PSS MiB | 15.64 | 15.73 | 14.62 | 16.02 | -0.019 |
| lk2 agent count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk2 agent fds | 17.00 | 15.00 | 12.00 | 17.00 | -0.096 |
| lk2 agent threads | 4.00 | 4.00 | 3.00 | 4.00 | +0.029 |
| lk2 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.18 | -0.000 |
| lk2 crun-shim PSS MiB | 10.18 | 6.09 | 1.99 | 10.18 | -0.113 |
| lk2 crun-shim count | 6.00 | 4.00 | 1.00 | 6.00 | -0.096 |
| lk2 crun-shim fds | 48.00 | 32.00 | 8.00 | 48.00 | -0.768 |
| lk2 crun-shim threads | 6.00 | 4.00 | 1.00 | 6.00 | -0.096 |
| lk3 MemAvailable MiB | 689.57 | 701.27 | 689.57 | 739.75 | +0.411 |
| lk3 agent CPU % | 0.10 | 0.05 | 0.00 | 1.32 | +0.004 |
| lk3 agent PSS MiB | 15.71 | 15.18 | 14.50 | 16.15 | +0.003 |
| lk3 agent count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk3 agent fds | 17.00 | 16.00 | 15.00 | 17.00 | -0.001 |
| lk3 agent threads | 4.00 | 3.00 | 3.00 | 5.00 | -0.009 |
| lk3 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.10 | +0.000 |
| lk3 crun-shim PSS MiB | 10.19 | 7.30 | 6.03 | 10.19 | -0.002 |
| lk3 crun-shim count | 6.00 | 5.00 | 4.00 | 6.00 | -0.001 |
| lk3 crun-shim fds | 48.00 | 40.00 | 32.00 | 48.00 | -0.012 |
| lk3 crun-shim threads | 6.00 | 5.00 | 4.00 | 6.00 | -0.001 |
| pods (all namespaces) | 12.00 | 16.00 | 12.00 | 16.00 | +0.002 |
| prober success % | 100.00 | 100.00 | 84.78 | 100.00 | -0.002 |

prober: 71936 ok, 38 failed (0.053 %) in 22 sample(s) with failures
