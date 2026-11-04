---
title: "The pod that waits: how Pending works, and the deadline that never passed"
description: "What a Pending pod means, how the scheduler decides whether it fits, and the bug that let a Job with a deadline wait for ever."
date: 2026-11-04
tags: [scheduling, jobs, controllers, pending]
docs: [multi-node, dev]
translation: es/el-pod-que-espera
---

A pod in `Pending` is not broken: it is waiting. Kubernetes uses it as an
implicit queue. If a pod asks for more than is free, it stays put until
something finishes and makes room. That wait is useful, but it has a catch
with Jobs that have a deadline. And in litek8s we found that the deadline,
while the pod waited, never passed.

This post explains how `Pending` is handled, what we measured on a real
cluster and what changed in 0.2.21.

## Two kinds of Pending

`Pending` is a pod phase, and it covers two different situations:

1. **Nobody has placed it.** No node has room, or none meets its rules
   (affinity, taints, selectors). The pod has no `nodeName` and its
   `PodScheduled` condition is `False`, reason `Unschedulable`.
2. **It has a node, but its containers have not started yet.** Usually the
   image is downloading: the container waits with reason
   `ContainerCreating`, or `ErrImagePull` if that fails.

`kubectl get pods` shows `Pending` for both. To tell them apart, look at the
condition and the container state:

```bash
kubectl get pod <pod> -o jsonpath='{.status.conditions[?(@.type=="PodScheduled")].message}'
kubectl get pod <pod> -o jsonpath='{.status.containerStatuses[0].state}'
```

This post is mostly about the first kind: the pod that does not fit.

## How the scheduler decides a pod fits

The scheduler does not look at how much CPU or memory is **used**. It looks
at how much the pods **asked for** (`resources.requests`) against what the
node offers (`status.allocatable`). If the requests of the pods on a node
plus the new pod's go over allocatable, the pod does not fit there.

In litek8s this lives in `src/kube/controllers/scheduler.rs`:

- `pod_requests` adds up the CPU and memory requests of every container, or
  takes the largest init container if it asks for more (init containers run
  one at a time).
- For each node it compares the requests with `allocatable` and notes why a
  pod does not fit: `Insufficient cpu`, `Insufficient memory`, or the
  missing extended resource.
- If no node will do, it sets `PodScheduled=False`, reason `Unschedulable`,
  with a message such as `0/1 nodes are available: Insufficient cpu.` If the
  pod has a higher priority than pods already placed, it tries to evict
  lower-priority ones.

litek8s's controllers are driven by the store's change feed: when a pod
finishes and frees its room, that change sets the scheduler going again, and
it retries the waiting pods.

A pod with no requests always fits, even on a saturated node. That is why
you should declare them: they are all the scheduler knows about your pod.
[The node that was too full](/blog/the-node-that-was-too-full/) tells what
happens when requests and real use drift apart.

## What we measured

On a one-node dev cluster with 7 allocatable CPUs we created four Jobs. Each
asks for 3 CPUs:

- `a` and `b` sleep 40 seconds.
- `c` sleeps 5 seconds.
- `dl` sleeps 5 seconds and has `activeDeadlineSeconds: 15`.

Two fit (6 of 7 CPUs). This is how they looked every 5 seconds:

| second | a | b | c | dl |
|---|---|---|---|---|
| 5 to 35 | Running | Running | Pending | Pending |
| 40 | Succeeded | Succeeded | Running | **DeadlineExceeded** |
| 45 | Succeeded | Succeeded | Succeeded | DeadlineExceeded |

Pods `c` and `dl` showed `0/1 nodes are available: Insufficient cpu.` When
`a` and `b` finished, `c` started on its own: the implicit queue works.

But `dl` had a 15-second deadline and stayed active until second 40.

## A Job's deadline counts from when it exists

`activeDeadlineSeconds` counts from the Job's `status.startTime`, written
when the controller first sees the Job, not when its pod starts. **Time
spent Pending counts.** On upstream Kubernetes, a Job with a 15-second
deadline whose pod does not fit fails at 15 seconds with
`DeadlineExceeded`, even though it never ran.

That has a practical consequence: if your system creates many Jobs at once
and lets the cluster queue them, the last ones may die of their deadline
before they run. Turning the deadline off does not fix it either: without
one, a pod that never fits waits for ever.

## Why it never passed on litek8s

litek8s's Job controller already did the arithmetic right: it compares
`startTime` with the current time and, once past, fails the Job and deletes
its pods. The problem was **when** it did it.

Controllers react to changes. A Job is looked at when it or its pods change.
A pod that is Pending because it does not fit does not change: it is not
placed, does not start, does not finish. So nothing brought the controller
back to the Job, and the deadline went unchecked. In the table, `dl` failed
at second 40 only because `a` and `b` finishing triggered a look.

Conditions that depend only on time passing need a timer. The controller
had one for two other things: the backoff between retries and
`ttlSecondsAfterFinished`. It was missing for the deadline.

## The fix

When it looks at an active Job with a deadline, the controller now asks to
look again when the deadline passes. If a retry was already due sooner, the
sooner one wins:

```rust
// Come back when the deadline passes: a pod left Pending (nothing
// fits) changes nothing, so no event would bring the controller back.
let deadline_left = i(&job, "/spec/activeDeadlineSeconds").and_then(|d| {
    let start = status.get("startTime").and_then(Value::as_str).and_then(util::parse_time)?;
    Some((d - (now - start).num_seconds()).max(1) as u64)
});
if let Some(left) = deadline_left {
    let left = Duration::from_secs(left);
    requeue = Some(requeue.map_or(left, |r: Duration| r.min(left)));
}
```

Each work key has at most one pending timer, so this does not pile up
retries.

To check it we used `litek8s apiserver --no-node`: a server with no nodes,
where every pod stays Pending. We created a Job with a 10-second deadline on
0.2.20 and on the branch with the fix, and looked at 14 seconds:

| version | Job | pods |
|---|---|---|
| 0.2.20 | active | 1 Pending |
| with the fix | `Failed`, `DeadlineExceeded` | 0 |

The test `an_active_job_is_requeued_for_its_deadline` covers it: an active
Job with a deadline goes back on the queue with the time it has left.

## What to do about it in your system

Pending is a good safety net, not a work queue. If you create Jobs with
deadlines in bursts, you have two options:

- **Keep the wait out of the deadline:** do not create the Job until there
  is room. That is what we did in xsec, a service that runs scans as Jobs:
  scans wait in a queue in its own database, and the API creates Jobs only
  while fewer than N are running. The Job, and its deadline, start when
  there is room.
- **Keep the wait in the deadline, on purpose:** if work that does not start
  within 10 minutes is no longer useful, a deadline that includes the wait
  is exactly what you want. From 0.2.21 litek8s enforces it like Kubernetes.

Either way, declare `requests`. They are all the scheduler uses to decide
whether a pod fits.

## Diagnosing a Pending pod

```bash
# Was it placed? If not, why.
kubectl describe pod <pod> | sed -n '/Conditions/,/Events/p'

# What a node's pods asked for, against what it offers.
kubectl describe node <node> | sed -n '/Allocated resources/,/Events/p'

# Jobs that ran out of time.
kubectl get jobs -o custom-columns=NAME:.metadata.name,REASON:.status.conditions[0].reason
```

If the pod has a node and is still Pending, the problem is almost always the
image: look at `containerStatuses[0].state.waiting`.
