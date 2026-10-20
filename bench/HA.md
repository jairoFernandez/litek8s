# HA control plane: 3 API servers (2026-10-20, x86_64/KVM)

How the replicated control plane of [docs/ha.md](../docs/ha.md) behaves:
correctness under leader kills and partitions, the compat corpus and the
conformance suite while the leader is killed, failover time, write latency
and memory, next to k3s with embedded etcd on the same VM shapes.

## Setup

`bench/ha.sh` on `omarchy` (AMD Ryzen 7 5800X, 16 threads, 31 GiB; host
filesystem btrfs): qemu + KVM inside a Docker container given `/dev/kvm`,
all files in a scratch directory. Five Ubuntu 24.04 amd64 guests on one
private LAN:

| VM | role | shape |
|---|---|---|
| lk1, lk2, lk3 | API servers, each also a node (crun 1.14.1) | 2 vCPU, 2 GiB, 40 GB disk |
| lk4 | agent node | 2 vCPU, 2 GiB, 40 GB disk |
| lk5 | the client: `litek8s lb` on 127.0.0.1:6443, kubectl, corpus, e2e.test | 4 vCPU, 4 GiB |

`BENCH_LAN_MCAST=1` (the LAN is a multicast group on the host's loopback;
the Python UDP switch the other benchmarks use stalled for seconds under
bursts) and `BENCH_DISK_CACHE=none` (O_DIRECT image files: a guest flush
costs what it costs on a disk, instead of writing back every dirty page of
the whole image from the host's cache). litek8s at the commit this file is in;
k3s v1.33.4+k3s1 (`--cluster-init`, embedded etcd,
`--disable=traefik,servicelb,metrics-server`). Clients reach either through
the same TCP load balancer (`litek8s lb`; `--tcp-check` for k3s, whose
`/readyz` wants credentials).

RESULTS_PLACEHOLDER
