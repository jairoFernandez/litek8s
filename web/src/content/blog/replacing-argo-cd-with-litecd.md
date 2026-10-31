---
title: "Replacing Argo CD's controller with litecd: 666 MiB to 100, and what it cost"
description: "How we swapped Argo CD's application controller for litecd on a production k3s cluster: the plan, the traps, the numbers, and what we gave up."
date: 2026-10-30
tags: [gitops, argo-cd, litecd, memory, migration]
docs: [litecd, ui]
translation: es/reemplazar-argo-cd-con-litecd
---

On a small cluster the most expensive thing running is often not your
application. On litek8s's own bench, a 2 GiB machine with one synced app,
the whole Kubernetes control plane took 90 MiB and Argo CD's seven pods
359 MiB. GitOps cost four times what Kubernetes did.

That number is why [litecd](https://github.com/ideascoldigital/litecd)
exists: one Rust binary that reads Argo CD's `Application`, `AppProject` and
`ApplicationSet` manifests unchanged and writes the same `.status`. This is
the story of building it and then putting it in place of Argo CD's
controller on a real production cluster, including the parts that did not
go to plan.

## Where Argo CD's memory goes

Argo CD's cost is not waste; it is design choices that suit large clusters:

- the application controller watches **every resource type the cluster
  serves** (79 watches on litek8s, 103 on k3s) and keeps the cluster in
  memory, so it can answer "what does this app own" instantly;
- seven processes (application controller, repo server, API server,
  ApplicationSet controller, notifications, Dex, Redis), each with its own
  Go runtime and caches.

litecd makes the opposite choices. It watches only the types that appear in
some app's manifests, drops objects it does not track as they arrive, runs
as one process, and shells out to `git`, `helm` and `kustomize` exactly as
Argo CD's repo server does, so rendering behaves the same.

## Being compatible, measured

"Reads Argo CD's manifests" means nothing without a referee. Before writing
the controller we built a compatibility corpus: sets of manifests with their
expected results, where the expected results only ever come from running
real Argo CD (v3.5.5). Every case must pass on Argo CD first; litecd then
has to produce the same objects and the same status. The corpus grew to 38
cases: sync waves, every kind of hook, multiple sources with `$ref` values,
`ignoreDifferences` with jq paths, Argo CD's own Lua health library
(vendored), ApplicationSet generators, sync windows, apps outside Argo CD's
namespace.

On a litek8s cluster with 20 Applications, side by side:

| | Argo CD (5 pods) | litecd (1 pod) |
|---|---|---|
| memory, 20 apps idle | 310 MiB | 6.9 MiB |
| install → ready | 28.7 s | 0.6 s |
| 19 apps at once → Synced + Healthy | 5.7 s | 3.3 s |

Good lab numbers. Production is a different animal.

## The cluster

A k3s cluster of 6 nodes, about 4 CPUs and 5 GiB allocatable each. Argo CD
v3.4 in HA: Redis HA behind haproxy, two repo servers, two API servers,
sign-in through OIDC. 59 Applications, ~690 managed resources: plain
directories, Kustomize with a remote base, local Helm umbrella charts from
one SSH git repository, and 13 charts from Helm repositories. Automated
sync, prune and self-heal on every app, sync waves, Helm hooks,
`ignoreDifferences`, Lua health checks. And the detail that matters most:
an app of apps that also installs Argo CD itself.

Argo CD's namespace used 1,322 MiB across 15 pods; its application
controller alone, 666 MiB.

## Step 0: ask the manifests

`litecd check` reads a directory of manifests and reports which features
they use and in which phase litecd supports each. Fed the GitOps repository
and the cluster's `argocd-cm`: 54 apps needed phase 1 features, 5 needed
phase 2, nothing unsupported. That made the next step worth doing.

## Step 1: observe without touching

The rule for anything new near production: first it writes nothing. litecd
got an `--observe` mode that renders and compares every Application as it
would, then logs where its result differs from the status Argo CD stored,
and never writes a status, a sync, a prune or a finalizer.

Run from a laptop, it reported differences that were not real: a chart
repository addressed by in-cluster DNS, and a checksum annotation that
changed because the laptop had Helm 4 and Argo CD runs Helm 3. Lesson:
observe where the controller will run.

So it went into the cluster as its own Deployment, with a ServiceAccount
that can read everything and `patch` (the diff confirms itself with a
server-side apply dry run). To make "writes nothing" a guarantee rather
than a promise, a ValidatingAdmissionPolicy denies any request from that
ServiceAccount that is not a dry run (`request.dryRun == true`). Then the
code can be wrong and production still cannot change.

Result: **58 of 59 apps identical** to Argo CD's stored status. The 59th
renders a Secret with Helm's `randAlphaNum`: every render differs. Argo CD
hides that by caching rendered manifests; litecd, with its render cache on
an `emptyDir`, syncs that app once each time it restarts. Not a bug in
either, but a behavior worth knowing before the switch.

## Step 2: the switch, and the trap in it

The plan was three scripts, dry run by default: a precheck (read-only:
nothing syncing, everything Synced and Healthy, the observer clean, a
snapshot of every Application and of Argo CD's replica counts), the
takeover, and a rollback.

The trap is the app of apps. If Argo CD manages its own installation and
you scale its controller to 0, something with self-heal on will notice the
StatefulSet drifted from git and scale it straight back up. Including,
once it takes over, litecd. So the order is:

1. Turn off automated sync on the root app and on the app that installs
   Argo CD, **while Argo CD still runs**, and check it stuck.
2. Scale `argocd-application-controller` to 0 and wait for its pod to go.
3. Start litecd.

Writing that order down produced a feature. Two controllers reconciling
the same Application fight forever, each undoing the other's status, so
litecd now **stands down by itself** while Argo CD's application controller
has replicas: it writes nothing for any Application and logs why, and takes
over within seconds of the controller's last pod disappearing. ApplicationSets
wait the same way for Argo CD's applicationset controller. An app annotated
`litecd.dev/manage: "true"` is litecd's anyway, for testing one app first.

Kept: the CRDs, every Application, the AppProject, repository and cluster
Secrets, `argocd-cm`, and Argo CD's API server, repo servers and Redis, so
the team kept its web UI and the GitHub webhook (which sets the refresh
annotation litecd honours). Then notifications (unconfigured), the
ApplicationSet controller (no ApplicationSets) and Dex (OIDC configured
directly) went to 0 as well.

What happened: litecd logged *Argo CD's application controller is gone:
taking over Applications*, the counts matched, the one random chart synced
once and cleaned up its hooks, and nothing was pruned.

## The numbers

Measured with metrics-server, sampled every 5 minutes for hours:

| Argo CD's namespace | before | after |
|---|---|---|
| memory used | 1,322 MiB, 15 pods | 594 MiB, 11 pods |
| memory reserved (requests) | 1,216 MiB | 608 MiB |
| CPU | ~293m | ~198m |
| the controller | 666 MiB | litecd ~100 MiB |

About 730 MiB of memory back, and 610 MiB of reservations the scheduler
can hand to something else, while keeping Argo CD's UI. On 5 GiB nodes that
is real room.

The honest footnote: litecd sat at 7 MiB in the lab and at ~100 MiB here.
It grows with what it renders (59 apps, Helm umbrellas, a git checkout
cache) and then stays flat; it never restarted. Six times less than the
controller it replaced, not fifty.

Deploys kept working through it: commits picked up by polling in 13 s to
2 min (that repository has no webhook), syncs from under a second to 49 s
with hooks, rollouts with every replica available the whole time, and no
memory pressure, evictions or OOM kills on the nodes.

## What the change cost

Swapping a controller is not free, and the list is short but real:

- **The UI's resource tree went empty.** argocd-server reads the tree of
  Deployments, ReplicaSets and Pods from a Redis cache that only Argo CD's
  controller fills. Lists, sync and health still worked; the tree did not.
- **The Diff tab** reads another cache of that controller: still empty.
- **No SSO, notifications, `argocd` CLI over gRPC** in litecd itself; here
  they were either unused or kept on Argo CD's side.
- **A random chart syncs once per restart**, as above.

For the tree, litecd grew a compatibility mode that writes it into Argo
CD's Redis in Argo CD's format. We shipped it **deprecated from birth**: a
separate image tag, off unless a flag is set, a warning in the log at
start, and a promise to delete it. It is coupled to an internal cache
format nobody promised to keep stable, and it proved the point quickly:
the first version wrote child nodes without `kind` and `version` (objects
from a watch carry no type), and Argo CD's UI crashed on them. We reverted
to the plain image and deleted the trees it had written; the next release fixed it and now
refuses to write any tree with a node it cannot describe completely. It
exists only until litecd has its own view of an app's resources, computed
from the Kubernetes API. Until then, `litek8s ui` shows Applications, their
resources and their diff with git from any cluster, through the Kubernetes
API alone.

## Try it

What took scripts and care on that cluster is now one line on yours:

```bash
curl -fsSL https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh | sh
```

On a cluster without Argo CD it installs everything. Next to an existing
Argo CD it installs only litecd itself, leaves Argo CD's Service,
ConfigMaps and CRDs alone, and waits for you to scale Argo CD's controller
to 0. To see the whole loop locally, `examples/litecd/up.sh` in the litek8s
repository builds a dev cluster with litecd and an app of apps in about a
minute.

What is left on that production cluster: removing Argo CD's remaining
workloads would free about 600 MiB more, and it waits for the one thing
the team still opens Argo CD for, a web UI.
