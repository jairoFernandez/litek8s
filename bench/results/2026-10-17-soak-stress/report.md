99 samples over 0.9 h

| series | first | last | min | max | slope/h (2nd half) |
|---|---:|---:|---:|---:|---:|
| API delete mean ms | 0.14 | 0.09 | 0.07 | 2.38 | -0.344 |
| API get mean ms | 0.26 | 0.15 | 0.05 | 391.44 | -0.869 |
| API list mean ms | 0.33 | 0.28 | 0.26 | 1.22 | -0.329 |
| API patch mean ms | 0.37 | 0.22 | 0.13 | 2.16 | -1.141 |
| GET /api/v1/namespaces/soak/configmaps ms | 0.40 | 0.31 | 0.21 | 21.83 | -0.635 |
| GET /api/v1/namespaces/soak/pods ms | 1.32 | 1.10 | 0.84 | 12.99 | -2.506 |
| GET /api/v1/nodes ms | 0.61 | 0.46 | 0.41 | 6.74 | -0.506 |
| GET /apis/apps/v1/namespaces/soak/deployments/web ms | 0.59 | 0.69 | 0.38 | 6.35 | -0.866 |
| container restarts | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| litek8s_agent_connections | 3.00 | 3.00 | 2.00 | 3.00 | +0.000 |
| litek8s_agent_streams | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| litek8s_apiserver_watches | 5.00 | 0.00 | 0.00 | 5.00 | -0.227 |
| litek8s_process_resident_MiB | 432.83 | 433.05 | 432.46 | 433.05 | +0.000 |
| litek8s_store_compactions_total | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| litek8s_store_feed_receivers | 9.00 | 4.00 | 3.00 | 9.00 | -0.227 |
| litek8s_store_history_MiB | 2.21 | 1.66 | 1.64 | 2.21 | -0.081 |
| litek8s_store_objects | 209.00 | 213.00 | 207.00 | 215.00 | -0.080 |
| litek8s_store_wal_MiB | 22.22 | 31.28 | 22.22 | 31.28 | +10.937 |
| litek8s_store_wal_records | 2375.00 | 8117.00 | 2375.00 | 8117.00 | +6934.025 |
| lk1 MemAvailable MiB | 281.25 | 304.42 | 278.28 | 309.83 | +22.267 |
| lk1 apiserver CPU % | 0.10 | 0.10 | 0.10 | 8.72 | -0.098 |
| lk1 apiserver PSS MiB | 429.89 | 430.07 | 429.47 | 430.07 | -0.014 |
| lk1 apiserver count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk1 apiserver fds | 20.00 | 20.00 | 20.00 | 22.00 | +0.189 |
| lk1 apiserver threads | 3.00 | 3.00 | 3.00 | 3.00 | +0.000 |
| lk1 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| lk1 crun-shim PSS MiB | 6.16 | 6.15 | 6.14 | 9.85 | +0.291 |
| lk1 crun-shim count | 4.00 | 4.00 | 4.00 | 7.00 | +0.228 |
| lk1 crun-shim fds | 32.00 | 32.00 | 32.00 | 56.00 | +1.827 |
| lk1 crun-shim threads | 4.00 | 4.00 | 4.00 | 7.00 | +0.228 |
| lk2 MemAvailable MiB | 705.71 | 712.14 | 678.50 | 750.91 | -67.058 |
| lk2 agent CPU % | 0.10 | 0.06 | 0.00 | 0.46 | +0.227 |
| lk2 agent PSS MiB | 18.29 | 15.83 | 14.97 | 18.38 | +0.687 |
| lk2 agent count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk2 agent fds | 16.00 | 18.00 | 12.00 | 19.00 | +11.845 |
| lk2 agent threads | 3.00 | 3.00 | 3.00 | 3.00 | +0.000 |
| lk2 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.03 | -0.021 |
| lk2 crun-shim PSS MiB | 7.44 | 9.84 | 2.04 | 11.06 | +14.618 |
| lk2 crun-shim count | 5.00 | 7.00 | 1.00 | 8.00 | +11.845 |
| lk2 crun-shim fds | 40.00 | 56.00 | 8.00 | 64.00 | +94.761 |
| lk2 crun-shim threads | 5.00 | 7.00 | 1.00 | 8.00 | +11.845 |
| lk3 MemAvailable MiB | 715.58 | 728.11 | 674.89 | 738.25 | -0.242 |
| lk3 agent CPU % | 0.00 | 0.00 | 0.00 | 0.37 | -0.156 |
| lk3 agent PSS MiB | 17.58 | 15.36 | 15.12 | 17.88 | +0.369 |
| lk3 agent count | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| lk3 agent fds | 15.00 | 15.00 | 15.00 | 18.00 | -0.276 |
| lk3 agent threads | 3.00 | 3.00 | 3.00 | 3.00 | +0.000 |
| lk3 crun-shim CPU % | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |
| lk3 crun-shim PSS MiB | 6.08 | 6.13 | 6.07 | 9.78 | -0.315 |
| lk3 crun-shim count | 4.00 | 4.00 | 4.00 | 7.00 | -0.276 |
| lk3 crun-shim fds | 32.00 | 32.00 | 32.00 | 56.00 | -2.212 |
| lk3 crun-shim threads | 4.00 | 4.00 | 4.00 | 7.00 | -0.276 |
| pods (all namespaces) | 15.00 | 15.00 | 15.00 | 16.00 | -0.040 |
| prober success % | 100.00 | 100.00 | 73.68 | 100.00 | +1.031 |

prober: 2840 ok, 12 failed (0.421 %) in 7 sample(s) with failures
