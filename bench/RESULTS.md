# litek8s vs k3s on a small node — baseline

Baseline before replacing the Docker runtime (crun/litecontainer) and moving to a
push-based resident agent. Raw data:
[`results/2026-10-16-t4g-micro-shape/`](results/2026-10-16-t4g-micro-shape/)
(`report.md` there is the full generated report).

## Headline

One fresh guest per system: Ubuntu 24.04 arm64, **2 vCPU, 1 GiB RAM, no swap**
(the shape of an AWS t4g.micro). All three run the same `nginx:alpine` workload
with the same limits (500m CPU / 128Mi) and the same TCP readiness probe.

| metric | litek8s + Docker | k3s minimal¹ | k3s default |
|---|---|---|---|
| Idle orchestrator memory, 0 workloads (PSS) | **128 MiB** | 572 MiB | 693 MiB |
| Idle orchestrator memory, 5 nginx (PSS) | **188 MiB** | 603 MiB | 698 MiB |
| Guest RAM left (MemAvailable), 5 nginx | **627 MiB** | 321 MiB | 59 MiB |
| Idle CPU, 0 workloads (whole guest, % of one vCPU) | **0.9 %** | 4.2 % | 6.3 % |
| Idle CPU, 5 nginx (whole guest, % of one vCPU) | 5.8 % | **5.2 %** | 20.4 % |
| Apply → first ready container (median of 10, warm image) | **0.16 s** | 0.79 s | 1.75 s |
| Disk used after install (no workload images) | **285 MiB** | 406 MiB | 1,064 MiB |
| Orchestrator binary | 22.6 MiB | 68.1 MiB | 68.1 MiB |

¹ `--disable=traefik,servicelb,metrics-server,local-storage`: only CoreDNS left as an addon.
This is the real competitor; the default install barely fits 1 GiB (59 MiB left with 5 pods,
and part of its 20 % CPU is the kernel reclaiming memory).

Memory breakdown (PSS, MiB, 0 → 5 workloads):

| | control plane | runtime | addons |
|---|---|---|---|
| litek8s | litek8s serve 11 → 14 | dockerd 73 → 89, containerd 44 → 58, 5 shims 28 | – |
| k3s minimal | k3s-server 400 → 402 | containerd + shims + pause 117 → 144 | CoreDNS 55 |

**Read:** litek8s wins memory (3–4.5× less than k3s minimal), disk, startup latency and the
empty-cluster CPU. It already **loses on CPU at 5 workloads**, and that gap grows with every
deployment, because of its own polling (overhead #2 below), not because of Docker.

## Remote node over SSH (litek8s only)

Control plane in one guest, the node in a second guest of the same shape, reached through
`ssh` with ControlMaster exactly as `SshNodeClient` does it.

| RPC path | mean | p95 | CPU on control plane | CPU on node |
|---|---|---|---|---|
| `ssh` mux running `true` (transport floor) | 4.99 ms | 5.52 ms | 4.4 ms | 2.2 ms |
| `ssh` mux + `litek8s agent` exec, `Ping` | 7.22 ms | 8.04 ms | 4.4 ms | 4.8 ms |
| `ssh` mux + `litek8s agent` exec, `ListContainersForDeployment` | 7.36 ms | 8.09 ms | 4.6 ms | 5.0 ms |
| same requests straight to the resident `agent --serve` socket | **0.23 ms** | 0.32 ms | – | – |

RPC census with 5 nginx deployments on the SSH node, steady state (nothing changing), 60 s:

| request | per second | loop |
|---|---|---|
| `StatsSnapshot` | 7.50 | cluster snapshot (1 per deployment per refresh) |
| `ListContainersForDeployment` | 2.58 | reconcile (1 per deployment per pass) |
| `ProbeContainerTcp` | 2.52 | reconcile (1 per deployment per pass) |
| `Ping` | 1.62 | cluster snapshot (1 per node per refresh) |
| `ListManagedContainers` | 0.50 | reconcile (orphan cleanup, 1 per node per pass) |
| `HostResources` | 0.18 | cluster snapshot (5 s cache) |
| **total** | **14.9 RPC/s** | **11.2 RPCs per reconcile pass**, one pass every 2.0 s |

That traffic costs **13.3 % of a vCPU on the node** (vs 5.8 % for the same 5 deployments on
litek8s's local node) and **7.8 % on the control plane**, 5.8 % of it in forked `ssh` clients.

## Biggest litek8s overheads, in order

1. **Docker is about 90 % of the footprint.** At idle dockerd (73 MiB) and containerd
   (44 MiB) are ten times the size of litek8s itself (11 MiB), and the docker.io packages
   are 254 of the 285 MiB on disk. dockerd sits on top of containerd and is pure overhead:
   dropping it alone would save 73–89 MiB, as much as k3s's whole embedded containerd
   (52–105 MiB). Replacing dockerd + containerd with crun/litecontainer is the largest single
   win for memory and disk.

2. **Polling makes the runtime work 15× harder.** Going from 0 to 5 deployments takes
   dockerd + containerd + shims from 0.34 % to 5.27 % of a vCPU, while litek8s serve only goes
   from 0.47 % to 0.59 %. litek8s causes that runtime load itself:
   - The cluster snapshot loop calls `StatsSnapshot` (`docker stats`) for **every deployment
     on every refresh**. It refreshes on its 1 s ticker *and* after every reconcile pass, so
     about 1.6 times a second, or 7.5 stats calls/s for 5 deployments, half of all RPCs.
     Containerd's `write()` traffic, mostly socket IPC, was 12.6 MiB/min in that window.
   - Every 2 s pass lists containers and inspects + probes each deployment, even when nothing
     has changed.

   All of it is O(deployments) per second, with no change needed to trigger it. Cheap fixes:
   collect stats only while `top`/snapshot clients are actually watching, drop the
   post-reconcile snapshot refresh, do one list per node per pass instead of one per
   deployment, and back off the probe once a replica is Ready. Event-driven is better still.

3. **SSH fork/exec per remote RPC: 31× the latency, about 9 ms of CPU per call.** Each call
   forks `ssh` on the control plane (about 4.4 ms of CPU). On the node it costs an sshd
   session (2.2 ms) plus an exec of the 23 MiB `litek8s agent` proxy (about 2.6 ms more),
   which then forwards to the resident `agent --serve` that already exists. The resident
   socket answers the same request in 0.23 ms vs 7.2 ms. Calls in a reconcile pass are
   **sequential**, so a pass costs 11.2 × (RTT + ~7 ms). That is about 80 ms here; with 5
   deployments over a 50 ms WAN it is about 0.6 s, and somewhere around 30–40 deployments it
   no longer fits the 2 s tick (an estimate from the measured per-call cost, not measured).
   The resident agent and its TCP listener already exist; a persistent connection, or a node
   push, removes the ssh + exec entirely.

4. **The RPC count grows with deployments, even at steady state.** That is 2 RPCs per deployment
   per reconcile pass plus 1 per deployment per snapshot refresh. One batched "node status"
   call (all containers + probe results + stats) would make it about 1 per node per pass.

5. **state.json is not an idle cost.** In the 5-minute steady-state window litek8s serve
   wrote **0 bytes**: the store persists only when something changes. The file is 30 KB with
   5 deployments, against 9–19 MB of k3s SQLite plus 1.6–2.6 MiB/min of `write()` traffic from
   k3s-server. The full pretty-printed rewrite (including up to 250 events) will cost during
   churn: restarts, rollouts, flapping probes. This benchmark does not exercise churn.

6. *Minor:* each pass proposes `SetReplicaAssignment` + `SetReplicaStatus` through Raft for
   every replica, even when they are unchanged. The log is in memory and compacted by
   openraft's default snapshot policy, and all of litek8s serve stays under 0.6 % CPU, so it is
   low priority.

## Method and caveats

- **Host:** guests run under qemu + HVF on an Apple M3 Max. The memory and disk numbers carry
  over to a real t4g.micro. **The CPU percentages will be higher on Graviton2**, whose cores
  are slower (expect roughly 2–3×; not verified), but the ratios between systems should hold.
  To rerun on real instances, point `BENCH_HOST_<name>` at fresh hosts (see `vm.sh`). No
  cloud resources were created for this run.
- **Versions:** litek8s `78fc083` (release build, linux/arm64), docker.io
  29.1.3-0ubuntu3~24.04.2 (Ubuntu's package, same as `infra/terraform/userdata.sh`),
  k3s v1.36.5+k3s1 (stable channel on 2026-10-16).
- **Memory** is the sum of PSS (shared pages split fairly) per process. Processes are grouped
  by cgroup and command name: control plane = `litek8s serve` / `k3s-server` (k3s's single
  process also holds the apiserver, scheduler, kubelet, kube-proxy and kine/SQLite); runtime =
  dockerd/containerd/shims/pause; addons = non-nginx pods. "MemAvailable" is the kernel's
  estimate and treats file-backed pages (Go binary text) as reclaimable, so litek8s's drop in
  MemAvailable at idle (56 MiB) is smaller than its PSS (128 MiB).
- **CPU** is cumulative cgroup `cpu.stat` (so short-lived children such as ssh, iptables and
  agent execs are counted) plus per-process ticks, over a 300 s window that starts 60 s after
  the system settles. "Whole guest busy" is everything, kernel included.
- **Time to ready:** clock starts before the CLI `apply` (`litek8s apply` / `k3s kubectl
  apply`) and stops when the system reports the replica Ready (litek8s socket poll every 50 ms;
  for k3s, a `kubectl get -w` started before the clock). The image is pre-pulled; pull time
  is excluded. k8s uses `periodSeconds: 2` to match litek8s's 2 s tick; its bimodal
  1.7 s / 3.1 s times are consistent with a first probe that misses by one period. litek8s probes on the
  Docker start event, which is part of why it is faster.
- **Fairness tweaks:** the k8s manifests set `requests: 50m/32Mi`. Without them, requests
  default to the limits and 5 × 500m do not fit on 2 vCPU; litek8s does no CPU admission.
  No hostPorts, so no docker-proxy processes.
- **Remote RPC:** the guests reach each other through qemu user-mode networking on the
  same host, so there is no real network RTT. The per-call CPU and fork/exec costs are what
  this measures; a WAN adds its RTT to every call. The census uses a `/bin/sh` shim as `--remote-binary`. It is a plain `exec`
  except while a flag file exists; the CPU window ran with the flag off.
- Each window was run once. There are no repeat statistics on the idle numbers.

## Rerun

```bash
bench/run.sh                        # all phases on fresh local VMs, ~55 min
bench/run.sh litek8s rpc            # just litek8s single-node + remote-node phases
IDLE_SECS=60 SETTLE_SECS=10 bench/run.sh k3s-minimal   # quick look
python3 bench/report.py bench/results/<timestamp>      # tables from raw JSON
```

Needs Homebrew `qemu` (aarch64 + edk2 firmware) and Docker (to cross-build the linux binary,
same `rust:1.90` image as `node add --install`). It downloads the Ubuntu 24.04 arm64 cloud
image once into `bench/.work/`. Files: `vm.sh` (throwaway VMs, or attach to real hosts),
`run.sh` (phases), `node/probe.py` (in-guest sampler, time-to-ready and RPC timing),
`report.py` (tables), `build-linux.sh` (cross-build).
