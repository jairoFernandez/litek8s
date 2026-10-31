# Speed benchmarks

`bench/run.sh` measures footprint on a small VM (memory, idle CPU, disk). These two
measure **speed** on the local machine, no VM and no Docker:

| bench | what | run |
|---|---|---|
| `benches/micro.rs` | criterion micro-benchmarks: store create/get/update/list, WAL write and replay, watch fan-out, merge/strategic patch, JSON | `cargo bench --bench micro` |
| `benches/api_load.rs` | in-process `apiserver` (WAL on disk, controllers on) under concurrent HTTP/1.1 keep-alive clients: ops/s and p50/p90/p99 per scenario | `cargo bench --bench api_load` |

## api_load

Scenarios: `create`, `get`, `patch` (merge patch), `list_selector` (labelSelector, 100 of
1000), `list_all` (1000 ConfigMaps), `list_table` (what `kubectl get` asks for),
`watch_latency` (create → watch event), `schedule` (Deployment of 100 replicas → every
Pod created and bound to one of 5 Nodes).

```bash
cargo bench --bench api_load                      # compare with bench/perf-baseline.json, exit 1 on regression
cargo bench --bench api_load -- list watch        # only matching scenarios
cargo bench --bench api_load -- --save-baseline   # accept the current numbers
```

Env: `PERF_SECS=5 PERF_CONC=32 PERF_OBJECTS=1000 PERF_REPLICAS=100 PERF_TOLERANCE=0.15
PERF_MEMORY=1` (no WAL). Last run: `target/perf/latest.json`. The baseline is per machine:
save it on the machine you compare on (it is checked in from an Apple Silicon laptop).

## micro

Criterion prints the change against the previous run. Named baselines:
`cargo bench --bench micro -- --save-baseline main`, then `-- --baseline main` on a branch.
Quick pass: `-- --warm-up-time 1 --measurement-time 3`.

## Workflow

1. `--save-baseline` on main.
2. Change code; rerun the scenario you target (`cargo bench --bench api_load -- list`).
3. Profile hot spots: `cargo flamegraph --bench api_load -- list_all` or `samply record`.
