---
title: "We ran the official Kubernetes conformance suite on litek8s: what passes, what fails, and why"
description: "413 of 419 conformance specs, against 416 for k3s. The six failures, the first run that scored zero, and what each one says about Kubernetes."
date: 2026-10-30
tags: [conformance, kubernetes, testing, compatibility]
docs: [numbers, limitations, crds, auth]
translation: es/conformance-de-kubernetes-que-falla-y-por-que
---

"Compatible with Kubernetes" is an easy claim to make. The Kubernetes
project ships a way to check it: the conformance suite, the
`[Conformance]` specs of the upstream `e2e.test` binary that certified
distributions must pass. We ran it against litek8s, a Kubernetes API
server in one Rust binary, and kept every result.

The score, Kubernetes **v1.33.13**, 419 specs:

| | passed | wall clock |
|---|---|---|
| k3s v1.33.4 (k3d, reference) | 416/419 | 39 min |
| **litek8s, macOS (Docker runtime)**, last complete run | **413/419** | 29 min |
| litek8s, Linux (crun runtime), the specs macOS cannot run | 9/9 | 1 min |

Six failures. Five come from macOS, not from the API; one was a real bug,
since fixed. Here is each one, how we got there from a first run that
scored zero, and what the failures taught us about Kubernetes.

## The first run: 0 of 419

The first run never reached a test. It stopped in `BeforeSuite`, the setup
step, because the suite lists nodes with
`fieldSelector=spec.unschedulable=false` and litek8s returned an empty
list.

Our nodes did not have `spec.unschedulable` at all. In Kubernetes, an
absent field must match `false`. That rule is not written down as a rule;
it falls out of the Go types, where a missing boolean is just the zero
value. It was the first of many lessons in the same shape: **the
behaviour of Kubernetes is the behaviour of its Go code**, and a
reimplementation has to match the code, not the docs.

From there, each run fixed what the last one exposed:

| run | passed | wall clock |
|---|---|---|
| r1 | 0/419 | (stopped) |
| r2 | 271/419 | 77 min |
| r3 | 346/419 | 59 min |
| r4 | 388/419 | 40 min |
| r5 | 405/419 | 47 min |
| r6 | 412/419 | 27 min |
| r7 | 413/419 | 29 min |

## What it took

Four areas, roughly in the order the suite forced them on us.

**Wire formats.** kubectl and client-go talk protobuf, not JSON. In
protobuf, embedded structs are flattened, a few field names differ from
the JSON ones, and zero values (always on the wire) must be dropped unless
the Go field is a pointer. We ended up generating that list of pointer
fields from the Go types. Again: the Go code is the spec.

**API machinery.** A CEL interpreter for ValidatingAdmissionPolicy and
webhook `matchConditions`; CRD schemas with pruning, defaults, validation
and OpenAPI; conversion webhooks; list continue tokens that expire; watch
resume; graceful, foreground and orphan deletion; ResourceQuota and
LimitRange; service account tokens; and API aggregation.

**Controllers.** ReplicationControllers, ControllerRevisions for
StatefulSets and DaemonSets, Job failure and success policies,
PodDisruptionBudgets and eviction, scheduler preemption, taints.

**The node agent.** Lifecycle hooks, termination messages, ephemeral
containers, sysctls, the `/etc/hosts` and `resolv.conf` the kubelet
writes, service environment variables, container start order, hostPort
conflicts, session affinity, the `proxy` subresources, and SRV, PTR and
ExternalName records in the cluster DNS.

None of these is exotic. Each is something a workload somewhere depends
on, which is why the suite checks it.

## The failures, one by one

Per SIG, the last complete run against k3s:

| SIG | k3s | litek8s |
|---|---|---|
| api-machinery | 93/94 | 93/94 |
| apps | 59/60 | 60/60 |
| architecture | 1/1 | 1/1 |
| auth | 10/10 | 10/10 |
| cli | 17/17 | 17/17 |
| instrumentation | 4/4 | 4/4 |
| network | 43/43 | 38/43 |
| node | 89/89 | 89/89 |
| scheduling | 10/11 | 11/11 |
| storage | 90/90 | 90/90 |

### DNS over TCP (4 specs, macOS only)

Each DNS spec has a pod look up every name over UDP **and** over TCP.
People forget the TCP half, but a DNS client falls back to TCP when an
answer does not fit in a UDP packet, so the suite insists on it.

The cluster DNS answers TCP on Linux. On macOS, port 53 belongs to the
system's own resolver, mDNSResponder: binding the wildcard address fails
with `EADDRINUSE`, and binding a specific address needs root. UDP works,
so the UDP half passes and the TCP half fails. On Linux with crun, all the
DNS specs pass.

### HostPort (1 spec, macOS only)

Three pods ask for the same `hostPort`, each with a different `hostIP` or
protocol. In Kubernetes these do not conflict: a host port is really the
triple of address, port and protocol. On macOS, Docker Desktop and
OrbStack run containers in a VM whose addresses differ from the node's,
so a binding to the node's address cannot be made. It passes on Linux
with crun.

### A watch that fell behind (1 spec, fixed)

The last failure was ours: `Watchers should observe add, update, and
delete watch notifications`. A burst of writes outran the change feed and
the watch ended. A Kubernetes watch is a promise that you see every change
from a resourceVersion on, in order; dropping the connection breaks it.
Watches now catch up from the store's history instead, and the next run
passed the spec.

That next run, on the final code, failed only the five macOS specs in its
parallel pass (385/390). Its serial pass of 29 specs was cut short when
the host disk filled up and Docker stopped, so it has no complete score.
Those 29 specs passed in the two runs before it.

## k3s failed three too

The reference is not perfect either. k3s failed a `Namespaces` patch and a
DaemonSet spec on a timeout, and a preemption spec lost its connection.
Those runs shared the same Docker VM with litek8s runs, so they look like
load, not gaps. We mention it because a conformance score is a measurement
on a machine, not a property of the code alone.

## The suite is hard on the node

One spec creates Deployments in a loop until a CEL admission policy denies
them. Before litek8s evaluated those policies, the loop never stopped: it
churned through about 6,000 pods and wedged Docker. The node agent now
also sweeps, every minute, the containers of pods it no longer knows.

## What the score does not say

We would rather say this ourselves than read it in the comments:

- **The runs used an in-memory store.** Conformance does not test
  durability, and neither did we here.
- **Not covered by conformance and not tested here:** etcd-backed
  durability, HA control planes, cloud providers, Windows nodes.
- **No authorization during the runs.** The notes record that every
  SubjectAccessReview was allowed. RBAC enforcement has since shipped
  ([Authentication and RBAC](/docs/auth/)).
- **Conformance is not "my stuff runs".** For that we keep a separate
  corpus of kubectl and Helm workflows, including 47 popular charts
  installed with `--wait`; it passes 119/119, like k3s
  ([Numbers](/docs/numbers/)).
- A focused run of the CRD and server-side apply specs, conformance and
  not, passed 48/48, like k3s ([CRDs and server-side apply](/docs/crds/)).

The rest of what litek8s does not do is in
[Limitations](/docs/limitations/).

## Try it

The runner downloads `e2e.test` and `ginkgo` v1.33.13 and runs them from
the host the way the upstream conformance image does. From a checkout:

```bash
cargo build --release
target/release/litek8s apiserver --kubeconfig /tmp/litek8s.kubeconfig &
target/release/litek8s node-agent --server http://127.0.0.1:6443 \
  --node-name "$(hostname -s | tr A-Z a-z)-2" --runtime docker &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s \
  --focus 'Aggregator.*\[Conformance\]' --no-serial   # one spec
```

The second node agent is there because some specs (pod anti-affinity,
DaemonSets, preemption) need two schedulable nodes. Expect about half an
hour for the whole suite. The full write-up, with pointers to the raw JUnit results
and ginkgo logs, is in
[compat/CONFORMANCE.md](https://github.com/jairoFernandez/litek8s/blob/main/compat/CONFORMANCE.md).
If you find a spec that passes for us and should not, we want to hear
about it.
