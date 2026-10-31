---
title: "The node that was too full: why Kubernetes evicted our database"
description: "One node at 97% memory, a database evicted twice, and a scheduler that saw room. Requests against usage, and how we moved pods without downtime."
date: 2026-10-30
tags: [scheduling, memory, eviction, k3s, operations]
docs: [ui, litecd]
translation: es/el-nodo-demasiado-lleno
---

The page said nothing was wrong. Every node was Ready, no pod was Pending,
CPU sat at 12%. And yet the production database had been evicted the night
before, for the second time in a few weeks.

This is what we found on a production k3s cluster, six nodes of about 4 CPUs
and 5.2 GiB allocatable each, which we'll call A to F. The short version:
**Kubernetes places pods by what they ask for, and evicts them by what they
use.** When those two drift apart, a node can be full and look empty.

## The symptoms

Seen per node, as a share of allocatable memory (working set, from
metrics-server):

| node | A | B | C | D | E | F |
|---|---|---|---|---|---|---|
| memory used | **93%** | 81% | 45% | 71% | 62% | 52% |

Node A had sat between 92% and 97% for hours. The database, a
single-replica StatefulSet, lived there, and its eviction event was blunt:

```
The node was low on resource: memory. Threshold quantity: 100Mi,
available: ~40Mi. Container was using ~2.3Gi
```

It restarted cleanly and the scheduler put it right back on A. Weeks
earlier the same database had been evicted when a CI build with no memory
request grew to 3 GiB on its node, and image builds had been OOM-killed on
other nodes. A pattern, not bad luck.

## Why the scheduler saw room

The scheduler does not look at usage. It adds up **requests**. On A they
came to 4,250 of 5,327 MiB, 80%: from the scheduler's side, about a GiB to
spare. Each workload on A looked honest on its own:

| on node A | request | used | limit |
|---|---|---|---|
| database | 2 GiB | 1.9 GiB | 3 GiB |
| identity provider (1 replica, Recreate) | 768 MiB | ~610 MiB | |
| backend API (1 of 2 replicas) | 640 MiB | ~620 MiB | |

The problem hides in the first row. The database requested 2 GiB but was
allowed 3: a gigabyte of growth nobody had reserved. And "used" on a node
is more than its pods: across the cluster the pods' own working sets summed
to 12.5 GiB while the nodes reported 21.1 GiB used. The other 8.6 GiB is
page cache (much of it the database's own files), the kernel and k3s.

Then the kubelet's rule finishes the story: under memory pressure it evicts
first **the pods using more than they requested**. The database, every time
it grew past 2 GiB.

## The number that looked scary and wasn't

The cluster summary said memory limits were at **171%** of allocatable.
That sounds like overcommit at its worst, but it misleads in two ways:

- it is inflated by a few big limits far above real use: a CI runner
  (limit 4 GiB, idle ~220 MiB, jobs need 2–3 GiB), a local LLM server
  (limit 3 GiB, idle ~60 MiB, loads models on demand), workflow workers
  (3 GiB each, ~265 MiB used). Those are sized for real peaks;
- it only counts pods that **have** limits. The 41 pods with no memory
  limit, including two CI runners requesting 2 GiB, can grow without bound
  and are the bigger risk.

The views that told us more were per-node requests against usage, pods
sorted by usage minus request (the eviction candidates), and pods without
limits.

## What we did

**1. Free memory elsewhere.** This came first and was unrelated: the same
week we [replaced Argo CD's application controller with
litecd](/blog/replacing-argo-cd-with-litecd/), which freed about 730 MiB
used and 610 MiB reserved. The controller had been on node F, which became
the emptiest node, with about 2.4 GiB reservable.

**2. Move a pod, and watch the scheduler surprise us.** The backend API has
two replicas and `maxUnavailable: 0`, so deleting its pod on A costs
nothing. The replacement went to node B, taking it from 81% to 86%. The
scheduler does not pick the emptiest node: it scores nodes, and requests
plus spreading pointed at B.

**3. Move it properly.** Cordon what you don't want, delete, uncordon:

```bash
kubectl cordon node-a node-b
kubectl -n prod delete pod backend-api-<hash>     # → node C
kubectl -n auth delete pod identity-<hash>         # → node F, next to its database
kubectl uncordon node-a node-b
```

The identity provider has one replica and a Recreate strategy, so moving
it cost about 2 minutes 20 seconds of sign-in being unavailable. That is
the price of a single replica, and it was worth knowing up front.

Per node afterwards, and an hour later:

| node | A | B | C | D | E | F |
|---|---|---|---|---|---|---|
| before | 93% | 81% | 45% | 71% | 62% | 52% |
| after the move | 70% | 77% | 59% | 71% | 57% | 64% |
| an hour later | 70% | 81% | 57% | 68% | 56% | 69% |

**4. Fix the cause, in git.** Prepared but not yet applied:

- the database's memory **request equal to its limit** (3 GiB). It can then
  never use more than it asked for, so it stops being the kubelet's first
  choice to evict, and the scheduler keeps the full 3 GiB free on its node;
- a 4 GiB limit on the CI runners that had none.

Deliberately left alone: the CI runner's 2 GiB request and the workflow
workers' limits, because the real peaks need them. Still pending: applying
the database change (a single-replica restart is a short outage, so it gets
a window), limits on the remaining system pods, and possibly a seventh
node.

One honest twist: these fixes **raise** the scary 171%, since pods that had
no limit now get one. The number goes up while the risk goes down.

## What it taught us

- **Requests drive placement; usage drives eviction.** Watch the gap. A pod
  running near or above its request on a busy node is the next eviction.
- **For what you cannot lose, request what you can use.** Request equal to
  limit makes a pod Guaranteed-like for memory: it is never over its
  request.
- **"Limits at 171%" says little.** Pods without limits say more.
- **Moving a pod is cordon, delete, uncordon**, and check the replica count
  and strategy first.

## And for litek8s

What made node A visible was `litek8s ui`'s Home view for clusters that are
not litek8s: per-node memory as a share of allocatable, on the k3s cluster
itself. This incident wrote its next features: requests and usage side by
side per node, a list of eviction candidates (usage above request), pods
without limits, a plain explanation of what a limits percentage means, and
a guided "move this pod" that cordons the other nodes and warns when a
workload has one replica or a Recreate strategy. And for litek8s's own
scheduler, a drift check that flags pods living near their request, long
before the kubelet has to choose whom to evict.
