# Soak test: a 3-node litek8s cluster under churn

How litek8s's Kubernetes path behaves over a long run on several nodes:
whether anything leaks or grows without bound, and whether node failures
heal. Script: [`soak.sh`](soak.sh) (summary: [`soak_report.py`](soak_report.py)),
cluster: [`cluster.sh`](cluster.sh).

## Setup

- 3 local VMs (`bench/cluster.sh up 3`, qemu + HVF on an M3 Max): Ubuntu
  24.04 arm64, **2 vCPU, 1 GiB RAM, no swap** each, on one private LAN
  (192.168.77.0/24, through `bench/lanhub.py`). lk1 runs `litek8s
  apiserver` (with its local node, data dir on disk), lk2 and lk3 run
  `litek8s agent --join`. Pods run on crun; pods of different nodes talk
  over the WireGuard overlay. Every litek8s process is a systemd unit with
  `Restart=always`, so a VM booted again rejoins by itself.
- Grace period 40 s (`--node-monitor-grace-period`), default tolerations
  300 s.
- Workload (namespace `soak`): nginx Deployment ×6 with a readiness probe
  and a ConfigMap volume behind a Service; a DaemonSet (busybox httpd); a
  StatefulSet ×2 with local-path volumes; a CronJob every minute that calls
  the Service by DNS; a prober Deployment that fetches the Service once a
  second and logs `ok`/`fail`.
- Churn: a ConfigMap update plus `rollout restart` every 5 min; a random
  workload pod deleted every 2 min; every hour a worker goes down and comes
  back, cycling through four kinds of outage on lk2/lk3 in turn:
  1. **kill-long**: hard power-off (SIGKILL of qemu) for 8 min, longer than
     grace + tolerations: the node turns Unknown, its pods are evicted and
     rescheduled, then it boots and rejoins;
  2. **kill-short**: power-off for 60 s: back before eviction;
  3. **freeze**: SIGSTOP of the VM for 2 min (a laptop that sleeps);
  4. **agent-restart**: `systemctl restart litek8s-agent` (containers keep
     running, the agent adopts them).
- Every sample (nominally 60 s; ~90 s in practice, the sampler itself takes
  that long over ssh): PSS, CPU time, fd and thread counts of every litek8s
  process on every node (API server, agents, crun shims), MemAvailable,
  `/metrics` of the API server (objects, WAL bytes/records, compactions,
  history, open watches, change-feed receivers, agent connections, request
  latency per verb), timed GETs on the server itself, pod phases and restart
  counts, and the prober's success rate.

## Result: 21.1 h, no leak, every outage healed

Run `results/2026-10-18-soak/` (samples, churn events, and the API
server's node-lifecycle log). Planned for 24 h; it ran **21.1 h** (17:08 to
14:11 UTC) and stopped because **the Mac itself rebooted** (`last reboot`:
14:12 UTC), taking every VM and the soak script with it. Nothing in the
cluster failed at that point: the last sample (14:11) shows lk2 Unknown in
the middle of a scheduled kill-long outage, lk1/lk3 Ready, prober 59/59.
Build: the branch at 4d24990 (before the compact-JSON store, 278cbfe, and
the shared-Docker label fix, 5c449fa, which came later).

| hours | API server PSS (MiB) | agent lk2 | agent lk3 | objects | WAL (MiB) | compactions | watch history (MiB) | open watches | GET pods (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0.25 | 27.6 | 15.6 | 15.8 | 73 | 1.3 | 0 | 1.33 | 0 | 4.64 |
| 1 | 28.7 | 15.9 | 15.9 | 81 | 5.1 | 0 | 1.44 | 0 | 1.16 |
| 3 | 29.9 | 15.5 | 15.0 | 81 | 15.4 | 0 | 1.44 | 0 | 1.46 |
| 6 | 30.1 | 15.2 | 15.9 | 81 | 2.2 | 1 | 1.45 | 0 | 1.28 |
| 9 | 30.4 | 15.5 | 15.4 | 81 | 17.4 | 1 | 1.47 | 0 | 10.59 |
| 12 | 30.5 | 15.8 | 15.2 | 81 | 5.0 | 2 | 1.46 | 0 | 1.53 |
| 15 | 30.2 | 15.5 | 15.0 | 81 | 20.1 | 2 | 1.45 | 0 | 1.06 |
| 18 | 30.5 | 15.2 | 15.6 | 81 | 7.6 | 3 | 1.44 | 0 | 1.01 |
| 21 | 31.1 | 15.7 | 15.2 | 81 | 22.3 | 3 | 1.45 | 0 | 0.95 |

Trends (least-squares slope over the second half, `soak_report.py`):

- **API server**: PSS 24.4 → 31.1 MiB, all of it in the first ~3 h
  (objects 65 → 81 as ReplicaSet and Job history filled up to their
  limits, watch history to its steady 1.45 MiB); second half **+0.05
  MiB/h**, i.e. flat. fds 19–24, 3–4 threads, CPU 0.07 % of one core.
- **Agents**: 15–16 MiB PSS each, slope ±0.02 MiB/h; fds and threads flat.
- **crun shims**: one per running container (~1.5 MiB each), counts follow
  the pods on each node; nothing accumulates.
- **Object log**: grows ~5 MiB/h with churn and was compacted 3 times
  (sawtooth between 2 and 28 MiB); objects stay at 81, open watches 0–4.
- **API latency** on the server: GETs of pods 0.7–1.5 ms, mean request
  latency per verb 0.1–0.3 ms, no upward trend.
- **Workload**: 0 container restarts, 250 rollouts (each done in ≤1 s with
  cached images), 627 pod deletions, 21 node outages. Prober: **71 936 ok,
  38 failed (0.053 %)**.

### Node outages

Times from the API server's log (`apiserver-lifecycle.log`):

| outage (×) | node marked Unknown after | pods evicted | back |
|---|---|---|---|
| kill-long, 8 min (6) | 30–38 s | at exactly +300 s, the default toleration (4 in all, plus 1 during the disk-full hang below; the churn had already deleted the others) | rejoined as soon as the VM booted, taints removed |
| kill-short, 60 s (5) | 32–40 s | none: back before 300 s, pods kept | as soon as booted |
| freeze, 2 min (5) | 32–37 s | none: pods kept | within 1 s of SIGCONT |
| agent restart (5) | never: containers kept running, the agent adopted them | none | at once |

The prober's failures (1–7 per minute) all fall in the ~40 s between a node
dying and being marked Unknown, while its endpoints still take traffic
(Kubernetes behaves the same until the node controller notices).

### Incidents not caused by litek8s

- **20:35–22:16 UTC, lk2 dead.** The Mac's disk filled up (macOS log:
  APFS `ENOSPC` for qemu), and qemu pauses a VM whose disk write fails. lk2
  hung without a shutdown in its journal; litek8s treated it like any dead
  node (Unknown, taints, evictions) until the next scheduled kill-long
  rebooted it and it rejoined. A GET mean of 5 s in that minute is requests
  waiting on the stuck node.
- **13:22 UTC, lk3 Unknown for ~25 min.** lk3's kernel logged RCU stalls
  ("rcu_preempt kthread starved") at that time: the host was overloaded by
  other work in the hour before it rebooted, and the VM got no CPU. It
  rejoined by itself.
- **10:13 UTC, one rollout not done in 300 s.** It started the same second
  lk2 was powered off; a new pod bound to lk2 waited for the 300 s eviction.

### Fixed during the soak work

Found by the 1 h stress run (`results/2026-10-17-soak-stress/`, 30 s pod
kills, 2 min rollouts, an outage every 12 min) and the runs before it:
history bounded by bytes (a burst of CRDs pinned ~400 MB), WAL compaction
by size, `malloc_trim` after bursts, image GC on every pull and cleanup of
failed unpacks (2.3 GB left in `images/tmp`), a stream-table leak in the
agent, NodePort backends marked unroutable for good after a node outage,
one pending timer per controller key (periodic reconciles multiplied with
every event), Event TTL, and objects stored as compact JSON (the 124 CRDs
left by the Helm charts made the API server 234 MB; 80 CRDs now take 17
instead of 95 MiB).

