# GitOps with litecd

[litecd](https://github.com/ideascoldigital/litecd) is GitOps continuous
delivery in one small Rust binary that reads Argo CD's `Application`,
`AppProject` and `ApplicationSet` manifests unchanged and writes the same
`.status`. It is a separate project, built with litek8s as its first target:
on a cluster whose whole control plane fits in a few dozen MiB, Argo CD's
pods are the biggest thing running. litecd is that same GitOps loop in one
pod of single-digit MiB.

litek8s does not depend on litecd, and litecd uses only the public
Kubernetes API, so it runs on any conformant cluster. This page is about
running the two together.

## Why on litek8s

Side by side on one litek8s dev cluster with 20 Applications (each a
ConfigMap, a Service and a Deployment, automated sync with prune and
self-heal), from litecd's
[bench](https://github.com/ideascoldigital/litecd/blob/main/bench/RESULTS.md):

| | Argo CD v3.5.5 (5 pods) | litecd (1 pod) |
|---|---|---|
| Memory, no apps | 194 MiB | 2.2 MiB |
| Memory, 20 apps synced, idle | 310 MiB | 6.9 MiB |
| Memory, peak during a burst | 326 MiB | 9.4 MiB |
| CPU idle, 20 apps | 0.38 % of a CPU | 0.15 % |
| Install → ready | 28.7 s | 0.6 s |
| 19 apps at once → all Synced + Healthy | 5.7 s | 3.3 s |
| App created → Synced + Healthy (median) | 0.46 s | 0.46 s |

litek8s's [GitOps platform example](../examples/platform/README.md)
(12 Applications in waves, Helm charts, Kustomize, an app of apps that
manages itself, Gitea webhooks, CI, Atlantis) comes up with litecd in Argo
CD's place and passes its whole smoke test, with the CD controller at
28 MiB against Argo CD's 723 MiB.

Where the difference comes from: Argo CD's application controller watches
every resource type the cluster serves and keeps the cluster in memory, and
each of its processes carries its own Go runtime and caches. litecd watches
only the types that appear in some app's manifests, drops objects it does
not track as they arrive, and runs as one process.

## What it supports

The same manifests as Argo CD v3.5, checked against a compatibility corpus
whose expected results come from real Argo CD (38 cases, all passing):

- **Sources:** git directories (YAML, JSON, `recurse`, `include`,
  `exclude`), Helm charts in git or in a Helm repository, Kustomize (and its
  options), jsonnet; single or multiple sources with `$ref` value files.
- **Sync:** automated sync, prune, self-heal, sync waves, hooks (PreSync,
  Sync, PostSync, SyncFail, Helm's install/upgrade hooks, delete policies),
  retries, `PruneLast`, `ApplyOutOfSyncOnly`, `Replace`, server-side apply,
  `CreateNamespace`, `FailOnSharedResource`.
- **Diff:** `ignoreDifferences` (JSON pointers, jq paths, field managers),
  from the app and from `argocd-cm`.
- **Health:** built-in checks, Argo CD's own Lua library of them, and Lua
  checks from `argocd-cm`; the app-of-apps pattern.
- **ApplicationSet:** list, clusters, git (directories and files), matrix
  and merge generators; fasttemplate and goTemplate.
- **Projects and clusters:** AppProject restrictions and sync windows,
  Applications outside Argo CD's namespace, destinations in other clusters
  from Argo CD's cluster Secrets.
- **Webhooks:** `/api/webhook` for GitHub, Gitea, Gogs and GitLab behind a
  Service named `argocd-server`, so webhooks made for Argo CD keep working;
  signatures checked against `argocd-secret` as Argo CD checks them.

Not there: Argo CD's web UI, the gRPC API the `argocd` CLI uses, SSO/Dex,
the notifications controller, config management plugins, and the SCM
provider and pull request ApplicationSet generators. To see whether your
manifests use any of them, run `litecd check` ([below](#check-your-manifests)).

## Install on a litek8s cluster

litek8s's dev clusters serve the Docker Engine API, so you can build the
image straight into the cluster's image store, no registry involved:

```bash
litek8s dev up                     # kubectl's context points at it
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
git clone https://github.com/ideascoldigital/litecd && cd litecd
deploy/build.sh                    # litecd:dev, lands in the cluster
kubectl apply --server-side -R -f deploy/install
kubectl -n argocd rollout status deploy/litecd
```

Or use a released image instead of building one:

```bash
kubectl apply --server-side -R -f deploy/install
kubectl -n argocd set image deploy/litecd litecd=ghcr.io/ideascoldigital/litecd:v0.2.0
```

`deploy/install` holds Argo CD's own CRDs (unchanged), RBAC, `argocd-cm` and
the litecd Deployment, all in the `argocd` namespace. From there it is the
usual GitOps loop:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: guestbook
  namespace: argocd
spec:
  project: default
  source:
    repoURL: https://github.com/argoproj/argocd-example-apps
    path: guestbook
    targetRevision: HEAD
  destination:
    server: https://kubernetes.default.svc
    namespace: guestbook
  syncPolicy:
    automated: {prune: true, selfHeal: true}
    syncOptions: [CreateNamespace=true]
```

```bash
kubectl apply -f guestbook.yaml
kubectl -n argocd get applications     # SYNC STATUS, HEALTH STATUS as with Argo CD
```

The same steps work on a litek8s server cluster
([Clusters over SSH](clusters.md)) with the released image.

## Moving from Argo CD

litecd keeps Argo CD's tracking (the `argocd.argoproj.io/tracking-id`
annotation, or the label `argocd-cm` asks for) and its field manager, so the
resources Argo CD synced stay owned after the switch, and moving back is the
same steps the other way.

1. **Try it first without writing anything.** `--observe`
   (`LITECD_OBSERVE=true`) compares every Application as litecd would and
   logs where its status differs from the one Argo CD stored. It can run next
   to Argo CD, even from a laptop with a kubeconfig:

   ```bash
   litecd controller --observe --cache-dir /tmp/litecd-observe
   ```

2. **Install litecd.** It will not fight Argo CD for an app: while Argo CD's
   application controller runs in its namespace, litecd writes nothing for
   any Application (no status, sync, prune, finalizer or webhook refresh) and
   logs that it stands down. ApplicationSets wait for Argo CD's
   applicationset controller in the same way. An Application annotated
   `litecd.dev/manage: "true"` is litecd's either way; Argo CD still acts on
   it too, so use that only for a test app.

3. **Scale Argo CD's workloads to 0, or uninstall them, keeping its CRDs,
   Applications and Secrets.** litecd takes over as soon as the last
   controller pod is gone, and logs it.

## In `litek8s ui`

The terminal UI's **Apps** view (`0`) lists Applications run by Argo CD or by
litecd, names which controller runs them and its version, and syncs,
refreshes and diffs them through the Kubernetes API alone: see
[Apps: Argo CD and litecd](ui.md#apps-argo-cd-and-litecd). `D` shows the diff
with git, which litecd publishes for each app out of sync as a Secret
`<app>.diff` next to it; force and terminate are Argo CD only.

## Check your manifests

`litecd check` reads a directory of manifests and reports which features
they use and whether litecd supports each:

```bash
cargo run -- check -f path/to/gitops     # in a litecd checkout
```

## More

- [litecd's README](https://github.com/ideascoldigital/litecd#readme) and
  its [plan and design](https://github.com/ideascoldigital/litecd/blob/main/docs/spec.md).
- [`e2e/platform.sh`](https://github.com/ideascoldigital/litecd/blob/main/e2e/platform.sh):
  litek8s's platform example with litecd in Argo CD's place, end to end.

litecd is not affiliated with the Argo project; "Argo" and "Argo CD" are
trademarks of The Linux Foundation.
