---
title: "You don't need a space shuttle to ship your app"
description: "Why litek8s is so fast, what that speed costs, and why starting small on the real Kubernetes API lets you migrate later without rewriting anything."
date: 2026-10-29
tags: [opinion, performance, small teams]
docs: [numbers, alongside, auth, limitations]
translation: es/no-necesitas-un-transbordador-espacial
---

There is a conversation that comes up in almost every small team: "Should
we use Kubernetes?". And the answer tends to land at one of two extremes.
Either you set up EKS or GKE with their VPC, IAM, node groups, ingress
controller and Prometheus and Loki stack, and pay in money and hours for a
system designed for hundreds of teams. Or you give up and end up with
`docker compose up` on a VM and a deploy script nobody wants to touch.

My premise is simple: **I don't need a space shuttle to ship my app**. I
need Deployments, Services, rollouts, config, secrets, Helm, maybe Argo CD,
metrics and logs. And I want to be able to move to something enterprise,
the day the business asks for it, without rewriting anything.

This post is an honest look at why litek8s is so fast, what that speed
costs, and why I think starting small on the real Kubernetes API is a
reasonable bet.

## The numbers first

All of this is measured with the scripts in the repository, against k3s
(the reference "lightweight" distribution), kind and k3d. Setup, raw data
and fine print in [Numbers](/docs/numbers/).

| | litek8s | reference |
|---|---|---|
| Whole control plane, idle, no pods (1 GiB x86_64 VM) | **23 MiB** | k3s minimal: 567 MiB |
| Whole orchestrator with 5 nginx pods | **33.5 MiB** | k3s minimal: 579 MiB |
| Argo CD + one synced app, orchestrator memory | **90 MiB** | k3s minimal: 900 MiB |
| `dev up` → node Ready, Linux / macOS | **0.17 s / 1.28 s** | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s |
| GitHub Actions job: e2e of a Helm chart | **21 s** | kind: 79 s |
| Metrics, logs and alerts, added memory | **+1.8 MiB** | kube-prometheus-stack + Loki: 1,371 MiB |
| Official Kubernetes v1.33 conformance | **413/419** | k3s: 416/419 |

These are not lab numbers for a tweet: the memory difference is what is
left for your pods on a cheap VM or a Raspberry Pi.

## Why it is fast

It would be easy to say "because it is written in Rust". It would be
wrong, or at least far from the whole story. Rust helps (no GC, a static
binary, predictable memory), but most of the gain comes from **what
litek8s does not do**.

### 1. One process instead of a fleet

A classic Kubernetes is several processes talking over the network: API
server, etcd, controller-manager, scheduler, kubelet, kube-proxy, CoreDNS,
containerd. Each with its own runtime, caches, watches and JSON/protobuf
serialization back and forth.

In litek8s the API server, the object store, the controllers, the
scheduler, DNS, the service proxy and the node agent are **one process**.
The controllers do not make an HTTP request to update a Deployment's
status: they write to the store directly. Measured while Argo CD syncs an
app, k3s serves almost twice as many API requests (11.5 vs 6.4 per second)
because its kubelet and controller-manager write status and events through
it.

### 2. No etcd

The state lives in memory with an append-only write-ahead log on disk. For
one node or a handful of them, that is all it takes. When you need high
availability, three or five servers replicate that same store with Raft,
without bringing in a separate etcd ([HA](/docs/ha/)).

### 3. Starting a pod is cheap

In k3s a pod goes through kubelet → containerd → a *pause* container →
shim → runc. In litek8s, on Linux, starting a pod is an `unshare`, an
`ip -batch` and `crun create/start` per container. There is no pause
container: the pod's network and IPC namespaces live as bind mounts. Each
container has a ~1.5 MiB shim, and exits arrive through a pidfd, so
**nothing polls**: the idle runtime uses 0 % CPU.

The result: a `helm install` of Argo CD up to its 7 pods Ready takes 19.9 s
against 27.1 s on k3s, and a re-synced replica change takes 1.0 s against
1.9 s.

### 4. What you would normally install next to it is inside

Cluster DNS, the service proxy, `metrics.k8s.io` for `kubectl top`, a
time-series database, log search and alerts. Not as extra pods, but as part
of the same binary. That is why "observability" costs 1.8 MiB and not
1.3 GiB.

## What that speed costs

An honest look has to include this section. If litek8s were better at
everything, that would be suspicious.

- **Authentication and RBAC are simpler than upstream's.** Servers
  authenticate clients (service account tokens and client certificates)
  and enforce RBAC by default, but there is no OIDC or SSO and no way to
  revoke a single client certificate: to cut one off you rotate the client
  CA. For a team, this is the most important item on this list. Keep the
  API (port 6443) behind a firewall or a VPN anyway
  ([Authentication and RBAC](/docs/auth/)).
- **It is young, and it is one person's project.** 0.x releases: flags,
  APIs and on-disk formats may change without migration.
- **It is not 100 % kubelet.** Some behaviors are missing (`Unhealthy` and
  `BackOff` events), ValidatingAdmissionPolicy is accepted but not
  enforced, and `imagePullSecrets` is not used: nodes pull with their own
  registry configuration. The full list is in
  [Limitations](/docs/limitations/).
- **Not everything is faster.** Argo CD's first sync, which includes
  pulling an image, took 18.1 s against 15.7 s on k3s; most likely image
  pull and unpacking. And in an early version litek8s lost to k3s on CPU
  with 5 workloads because of its own polling, until that polling was
  removed. Measuring against your competitor also shows you where you lose.
- **No cloud load balancer.** A `LoadBalancer` Service is published on
  every node's ports, like k3s's servicelb. That works for an ingress
  controller on 80/443; it is not an ALB.
- **Conformance 413/419**, against 416/419 for k3s. Five of the six
  failures are macOS-only and pass on Linux.

## What a small team needs

Do the exercise with your team. Of everything a managed cluster brings,
what do you use every week?

What almost every team uses:

- Deployments with rollouts and rollback, replicas, health checks.
- Services, internal DNS, an ingress to the outside.
- ConfigMaps and Secrets.
- Jobs and CronJobs.
- Helm, and Argo CD if you do GitOps.
- Metrics, logs and an alert that wakes you up.
- More than one machine, and pods that move when one goes down.

All of that is in litek8s, with the real API: it is what the 119 cases of
the compatibility corpus (kubectl, CRDs, workloads, networking and 47 Helm
charts) and the official conformance suite measure.

What justifies the space shuttle:

- Several teams on the same cluster, signing in with the company's SSO,
  with an audit trail of who did what.
- Node autoscaling and control plane upgrades managed by someone else.
- Deep cloud integration: per-pod IAM, block volumes, managed load
  balancers.
- Compliance requirements that ask for a vendor and a signed SLA.

If your list looks like the first one, "80 % of the benefits of a cluster"
is not an exaggeration. It is not a measured figure, it is an estimate; but
look at what is in the remaining 20 %: mostly multi-team security and
delegated operations. Exactly the things you do not need in the first year,
and do need once there is money and people to pay for them.

## Starting small without getting stuck

What makes this strategy work is not that litek8s is fast, it is that **it
is the Kubernetes API**. There is no manifest format of its own, no
required annotations, no SDK. What you apply to litek8s you apply to EKS,
and the other way round ([Next to your clusters](/docs/alongside/)).

In practice, migrating looks like this:

1. Create the new cluster (EKS, GKE, whatever fits). Its context shows up
   in the same `~/.kube/config` next to litek8s's.
2. Point the same charts, or the same Argo CD Application, at the new
   cluster.
3. Switch DNS.
4. Once everything is stable, `sudo litek8s uninstall --purge` on your
   machines.

To be clear: **data is the hard part**, in litek8s and in any migration.
litek8s provisions local volumes on the node's disk; moving them to cloud
volumes is your job (a database dump and restore, rsync, whatever your app
uses). That is why I recommend that, if you can, the database lives
outside the cluster from day one (a managed Postgres, or a VM with
backups), and the cluster only runs stateless things. Then migrating is
little more than switching contexts.

## When it is time to migrate

Concrete signals, not a feeling:

- More than one team needs access to the cluster, and handing out client
  certificates by hand no longer scales: you want SSO and audit logs.
- A customer or an audit asks for an SLA or compliance you cannot sign
  yourself.
- You spend more time managing machines than shipping product.
- You need nodes to scale automatically with load.

If none of these sounds like you, you are probably still in the phase
where one binary, a few cheap machines and the usual API are enough.

## In short

litek8s is fast because it does less: one process instead of eight, an
in-memory store instead of etcd, pods without a pause container,
observability inside instead of next to it. That has a real price, above
all simpler access control than upstream's and the project's age, and it is
worth knowing before you use it with a team.

But the underlying idea does not depend on litek8s: **write against the
standard API, start with the least that works for you, and leave the door
open**. The day you need the space shuttle, your manifests already know how
to fly.
