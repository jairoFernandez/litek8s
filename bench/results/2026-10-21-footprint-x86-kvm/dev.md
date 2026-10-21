# `litek8s dev` memory, v0.2.1 (2026-10-21)

Release binaries of v0.2.1, each in a scratch `LITEK8S_HOME`, one cluster,
one node. Idle: 60 s after `dev up`, then 4 samples 20 s apart; 5 nginx:
`kubectl create deployment web --image=nginx:alpine --replicas=5` (pulled
from Docker Hub), Available, 60 s, then 4 samples 20 s apart. Samples
varied by < 2 MiB; the medians are below.

## Linux (omarchy: x86_64, Ryzen 7 5800X, 16 threads, rootless; crun 1.23.1, pasta 2026_10_02)

`dev-sandbox-mem.py`: the sandbox's systemd scope, "in use" =
`memory.current - file` (what `litek8s ui` and `dev list` count), plus
per-process PSS.

| | idle | 5 nginx |
|---|---|---|
| whole sandbox, in use | 44.5 MiB (anon 38.3, kernel 4.6) | 124.2 MiB (anon 101.7, kernel 21.0) |
| … pasta (RssAnon) | 25.1 MiB | 25.1 MiB |
| … the cluster's API server process (PSS) | 19.6 MiB | — |
| … the 5 pods' cgroups (a later sample, 122.6 in all) | — | 63.0 MiB (nginx: 1 master + 16 workers per pod, one per host thread) |
| … everything but the pods (litek8s, pasta, 5 shims) | 44.5 MiB | 59.6 MiB |
| sandbox CPU over 60 s, % of one core | 0.03 % | 0.06 % |

## macOS (M3 Max, macOS 27; `dev up --memory 2048`, 7 vCPU)

The VM process's phys_footprint (`footprint -p`, `dev list -o json`: what
Activity Monitor shows) and, inside the guest, `MemTotal - MemAvailable`
less the 1024 MiB the balloon holds (`balloon_inflate 262144` pages: the
guest may use 1024 of the 2048 MiB).

| | idle (fresh VM) | 5 nginx | pods deleted again |
|---|---|---|---|
| VM footprint on macOS | 238 MiB | 381 MiB | 384 MiB |
| used in the guest | — | 182 MiB (anon 37, slab 38) | 154 MiB |
| the cluster's cell (litek8s + pods, in use) | 30 MiB | 79 MiB | 44 MiB |
| VM process CPU over 60 s, % of one core | 0.45 % | 0.38 % | — |

The footprint keeps every page the guest touched (here: the image download
and unpack went through the guest's page cache) until the VM stops; see
docs/dev.md, "Memory on macOS". For comparison, a long-running
dev VM on the same Mac (4096 MiB, one cluster, up for a day) showed
1050 MiB in `dev list` at the time.

## Seen on the way (v0.2.1, rootless)

`dev down` / `dev delete` of the last cluster answered "the dev environment
closed the connection"; the delete left the cluster's data (a second delete,
with the environment stopped, removed it), and a `dev up` right after the
down came up without pasta (its pasta was gone within seconds; the old
environment was still stopping and shares `pasta.pid`). Waiting for the old
supervisor to exit avoided both.
