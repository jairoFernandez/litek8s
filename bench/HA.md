# HA control plane: 3 API servers (2026-10-20, x86_64/KVM)

How the replicated control plane of [docs/ha.md](../docs/ha.md) behaves:
correctness under leader kills and partitions, the compat corpus and the
conformance suite while the leader is killed, failover time, write latency
and memory, next to k3s with embedded etcd on the same VM shapes.

## Summary

| | litek8s, 3 servers | k3s v1.33.4, 3 servers (embedded etcd) |
|---|---|---|
| Linearizability (CRUD register workload, leader kills + partitions) | **linearizable**, 109,912 ops on VMs + 35 in-process runs | not run (the checker's client speaks plain HTTP) |
| Compat corpus, no faults | **118/119** | 119/119 (k3d reference, compat/README.md) |
| Compat corpus, leader killed every 2 min (each down 20 s) | **118/119** in 17 min (8 kills) | 114/119 in 48 min (18 kills) |
| Conformance (419 specs), no faults | **408/419** | 416/419 (k3d reference, compat/CONFORMANCE.md) |
| Conformance, leader killed every 5 min (5–6 kills) | **406/419** | not measured: the k3s servers ran out of memory and I/O first (below) |
| Write unavailability, leader killed and kept down 20 s | **4.7–5.9 s** (5 of 8), 11–15 s (3 of 8) | 10.1–10.2 s (5 of 7), 6.2 s (1) |
| Write unavailability, leader killed and restarted by systemd (2 s) | **2.2–2.5 s** | 5.2–10.3 s |
| Write latency, sequential PUTs through the LB, p50 / p99 | **4.1 / 13.6 ms** | 5.9 / 22.3 ms |
| Memory per server (PSS), idle | **32–36 MiB** | 393–401 MiB `k3s server` + 79–81 MiB containerd |
| Memory per server (PSS), right after the corpus under kills | **93–395 MiB** (6 runs) | 395–969 MiB `k3s server` + 90–92 MiB containerd |

Of litek8s's 11 conformance failures without faults, 7 are EmptyDir
`medium: Memory` specs (the crun runtime makes those volumes plain
directories, not tmpfs: a runtime gap, not HA), 2 are DaemonSet RollingUpdate
and the Guestbook app, and 2 were requests caught by a spurious election on
the slow VM disks. One server of the same shape for comparison: litek8s
0.20 ms p50 per write (main: 0.19 ms; no `fsync`), 23.7 MiB; k3s with SQLite
2.0 ms, 440 MiB + 100 MiB containerd (details below).

## Setup

`bench/ha.sh` on `omarchy` (AMD Ryzen 7 5800X, 16 threads, 31 GiB; host
filesystem btrfs): qemu + KVM inside a Docker container given `/dev/kvm`,
all files in a scratch directory. Five Ubuntu 24.04 amd64 guests on one
private LAN:

| VM | role | shape |
|---|---|---|
| lk1, lk2, lk3 | API servers, each also a node | 2 vCPU, 2 GiB, 40 GB disk |
| lk4 | agent node | 2 vCPU, 2 GiB, 40 GB disk |
| lk5 | the client: `litek8s lb` on 127.0.0.1:6443, kubectl, corpus, e2e.test | 4 vCPU, 4 GiB |

`BENCH_LAN_MCAST=1` (the LAN is a multicast group on the host's loopback;
the Python UDP switch the other benchmarks use stalled for seconds under
bursts) and `BENCH_DISK_CACHE=none` (O_DIRECT image files, so a guest flush
costs what it costs on a disk). litek8s at the commit this file is in, crun
1.14.1 on every node, default timers (300 ms heartbeats). k3s v1.33.4+k3s1
(`--cluster-init`, embedded etcd with k3s's own timers: 500 ms heartbeats,
5 s election timeout; `--disable=traefik,servicelb,metrics-server`),
containerd 2.0.5-k3s2, `--kube-apiserver-arg=anonymous-auth=true` so
`/readyz` is public. Clients reach either through the same TCP load balancer
(`litek8s lb`; `--https-check` for k3s).

The nemesis (`bench/ha.sh kill-leader`) SIGKILLs the leader's server process
only (`systemctl kill --kill-whom=main`; pods keep running): litek8s's Raft
leader, k3s's etcd leader (found with `etcdctl endpoint status`). With
`HA_DOWN_SECS=20` the server stays down 20 s, longer than an election;
without, systemd restarts it after 2 s.

The VM disks are the limiting factor of this test bed: under the
conformance suite's image pulls an `fdatasync` inside a guest takes 0.5–2.6 s
(qcow2 images on the host's btrfs, shared by five VMs). Both systems flush
their log before acknowledging; openraft 0.9's core waits for each flush, so
slow flushes delay heartbeats and occasionally elections, which is why
litek8s defaults to 300 ms heartbeats (100 ms gave 115/119 on the corpus and
frequent spurious elections here; see docs/ha.md "Disks").

## Correctness

**Linearizability.** `litek8s ha-check` runs a CRUD workload on ConfigMaps
(create/overwrite/delete/read of 6 keys by 8 clients, each mostly through
one server) and records every operation with its call and return time;
operations whose effect is unknown (timeouts, 504s, dropped connections)
may have happened at any later time or never. A Wing & Gong / porcupine
search checks the history per key (`src/kube/ha/lincheck.rs`).

- VMs, 240 s, leader killed (restarted by systemd) or partitioned away with
  iptables (8 s) every 15 s, 6 + 6 times: **109,912 operations** (30,811
  successful writes, 34,931 definite failures such as 404/409, 44,051 reads,
  119 unknown), **linearizable** (checked in 0.3 s).
- In-process (`tests/ha.rs`), three servers on loopback, leader partitioned
  4 s, killed (down 2 s) and killed-and-restarted at once, twice per run:
  linearizable in every run since the fix below (35+ runs, ~2,500–3,100
  operations each).

The checker found one real bug on the way: openraft lets a leader that
restarts before anyone else is elected resume its term, with the commit
index it last saved, and its read index could then miss writes it had
acknowledged (a DELETE answered 404 for an object created just before the
kill). The leader's read index now covers its whole log
(`restarted_leader_serves_fresh_reads` reproduces it).

**No acknowledged write lost or duplicated, watches resume**
(`failover_keeps_acknowledged_writes_and_watches_resume`): unique creates
and compare-and-swap increments through all three servers while the leader
is killed: every acknowledged create exists exactly once on every server,
the counter is between the acknowledged and acknowledged + unknown
increments, and a watch that hops servers whenever its stream breaks sees
every create exactly once, in resourceVersion order.

## Failover

`bench/node/haprobe.py` PUTs one ConfigMap ten times a second on one
keep-alive connection through the LB (as client-go does) and records each
write; a window is the time between the last successful write before a
kill and the first one after (so it includes up to 100 ms of probe
interval). Leader killed every 60 s (kept down 20 s) or every 30 s
(restarted by systemd):

| | litek8s | k3s |
|---|---|---|
| killed, kept down 20 s | 4.7, 4.8, 5.2, 5.3, 5.9 s; 11.2, 11.4, 15.3 s | 6.2, 10.1, 10.1, 10.1, 10.2, 10.2 s |
| killed, restarted after 2 s | 2.2–2.5 s (8 kills) | 5.2–10.3 s |

litek8s's floor is its timers: a follower calls an election 3 s (the
leader's lease) + 1.5–3 s after the last heartbeat. The slower ones waited on
slow log flushes (above). A leader restarted within 2 s takes its leadership
back before anyone else's timer fires. k3s waits out etcd's 5 s election
timeout, then its API servers reconnect.

## Write latency

30 s of back-to-back PUTs of one ConfigMap through the LB from lk5:

| | writes | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| litek8s, 3 servers | 6,956 | 4.1 ms | 4.4 ms | 13.6 ms | 107 ms |
| k3s, 3 servers | 4,568 | 5.9 ms | 8.9 ms | 22.3 ms | 69 ms |

The same against one server on the same VM shape (lk1, client lk5 straight
to it, no LB), litek8s at this commit and at main (220ee88), and k3s with
its default single-server datastore (SQLite through kine):

| one server | writes | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| litek8s, this branch | 146,621 | 0.20 ms | 0.21 ms | 0.24 ms | 31 ms |
| litek8s, main | 148,319 | 0.19 ms | 0.21 ms | 0.24 ms | 21 ms |
| k3s (SQLite) | 13,107 | 2.0 ms | 2.6 ms | 7.2 ms | 40 ms |

Single-server mode is unchanged (same numbers as main, same on-disk WAL).
The gap to three servers is durability more than the network: a single
litek8s server appends to its WAL without `fsync` (a write survives the
process dying, not the machine), while in HA mode a write is acknowledged
only once a majority has it on disk, with an `fdatasync` on each: ~4 ms on
these VMs, of which the flushes are most.

## Memory

PSS of each server's process (litek8s: `litek8s apiserver`, which also runs
the node's agent; k3s: `k3s server`, plus its containerd), idle, after the
failover runs:

| | lk1 | lk2 | lk3 |
|---|---|---|---|
| litek8s | 36.1 MiB | 35.8 MiB | 32.0 MiB |
| k3s server | 401.4 MiB | 392.9 MiB | 399.8 MiB |
| k3s containerd | 80.0 MiB | 79.3 MiB | 81.2 MiB |

One server, idle, right after the latency run: litek8s 23.7 MiB (main:
23.0 MiB); k3s 440.3 MiB `k3s server` + 100.5 MiB containerd. Raft, its log
and a second listener cost a three-server member ~10 MiB.

## Corpus and conformance under leader kills

**Corpus** (`compat/run.sh`, 119 cases, L0–L4 incl. 47 Helm charts) from
lk5 through the LB, leader killed every 2 min and kept down 20 s:

| | passed | wall clock | kills | not passing |
|---|---|---|---|---|
| litek8s | **118/119** | 17 min | 8 | `helm_grafana`: a namespace the case had just created read back as NotFound once |
| k3s | 114/119 | 48 min | 18 | `api_from_pod`, `helm_traefik`, `helm_nginx_ingress` (timeouts), `helm_argo_cd` (a retried install), `helm_trivy_operator` ("apiserver not ready") |

litek8s also passes 118/119 without faults; on k3s every case passes
without faults (the k3d reference run). The k3s LB checks `/readyz` over
HTTPS (anonymous `/readyz` enabled, as kubeadm clusters behind haproxy do):
with a plain TCP check it sent clients to servers still starting, and a
first run scored 81/119 on 503s.

**Conformance** (`compat/conformance.sh`, e2e.test v1.33, 419 specs):

| | passed | kills |
|---|---|---|
| litek8s, no faults | **408/419** | 0 |
| litek8s, leader killed every 5 min (down 20 s) | **406/419** | 5 |
| k3s, leader killed every 5 min | 22/390 (not a result) | 0 found |

litek8s's failures without faults: the 7 EmptyDir `medium: Memory` specs
(crun runtime gap), DaemonSet RollingUpdate, Guestbook, and two requests that
hit a spurious election on the slow disks (a reset connection, "timed out
confirming the leader"; that run predates the readiness fix of d63f761).
Under kills, besides the 9 that fail without faults: the sample aggregated
API server's discovery, pod logs in the OIDC discovery spec, and the two
Proxy specs, all paths relayed to another server (streams, proxy,
aggregation) that do not yet retry across a failover (a follow-up).

The k3s conformance run under kills is not a measurement of k3s: it started
right after the 48 min corpus run, with the k3s servers at 395–969 MiB on
2 GiB guests, and the cluster stopped serving (etcd's leader could no longer
be found, every spec timed out) while the host sat at 78 % iowait. On these
shapes (2 GiB servers) k3s did not get through a second long workload; that
is the comparison this test bed could make.

## What it took

Getting from "passes the in-process tests" to stable on five small VMs
needed, in order: Raft on its own runtime and listener (API bursts delayed
heartbeats), AppendEntries capped at 512 KiB (openraft gives each one a
heartbeat interval; batches of CRDs never fit), timeouts reported as
network errors rather than unreachable peers (openraft stops heartbeats
while backing off), a Raft log whose writes never block the core and whose
segments are preallocated (on ext4 a journal commit waits for every dirty
file of the disk), heartbeats with in-flight slots of their own, readiness
that tolerates a slow flush, and 300 ms heartbeats. The harness itself
needed a kernel LAN (BENCH_LAN_MCAST), O_DIRECT disks and 40 GB of disk
(chart images filled 12 GB and the Raft log hit ENOSPC).

Raw data: `bench/results/2026-10-20-ha-x86-kvm/`: `litek8s-final` (this
build: conformance under kills, lincheck, latency, failover in `down20/` and
`restart/`, memory), `litek8s-baseline` (conformance, no faults),
`litek8s-hb300` (corpus under kills, an earlier build with the same timers),
`k3s` (latency, failover, memory; `first-attempt/` the OOM kills of a run
with flannel on the wrong NIC), `k3s-v2` (corpus under kills behind a TCP
check), `k3s-v3` (corpus and conformance under kills behind the HTTPS
check), `single-*` (one server).
