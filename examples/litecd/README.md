# GitOps on litek8s with litecd

A local GitOps setup in one command: a litek8s dev cluster,
[litecd](https://github.com/ideascoldigital/litecd) (Argo CD's manifests,
one small pod) from its latest release, and an app of apps that deploys a
Helm chart and a Kustomize app from git. Guide and numbers:
[docs/litecd.md](../../docs/litecd.md).

```bash
examples/litecd/up.sh
```

| file | what |
|---|---|
| [`up.sh`](up.sh) | dev cluster `gitops`, litecd through its install.sh, `root.yaml`, then waits for every app to be Synced and Healthy and checks podinfo answers |
| [`gitops/root.yaml`](gitops/root.yaml) | the app of apps: syncs `gitops/apps/` from this repository |
| [`gitops/apps/00-project.yaml`](gitops/apps/00-project.yaml) | AppProject `demo` (wave -1): only these source repositories, only `demo-*` namespaces |
| [`gitops/apps/podinfo.yaml`](gitops/apps/podinfo.yaml) | podinfo, a Helm chart from a Helm repository with inline values, NodePort (wave 1) |
| [`gitops/apps/guestbook.yaml`](gitops/apps/guestbook.yaml) | the guestbook, a Kustomize directory in another git repository (wave 2) |

The manifests are plain Argo CD ones: the same directory works with Argo
CD in litecd's place.

## Make it yours

`root.yaml` reads `gitops/apps/` from GitHub, so to change the apps, fork
the repository, edit `gitops/apps/` there and point the root app at it:

```bash
REPO_URL=https://github.com/<you>/litek8s REVISION=main examples/litecd/up.sh
```

Every commit to that directory is synced within the polling interval
(`timeout.reconciliation` in `argocd-cm`, 180 s by default), or at once
with `litek8s ui` (`0`, then `r` to refresh) or:

```bash
kubectl --context litek8s-gitops -n argocd annotate application root argocd.argoproj.io/refresh=normal --overwrite
```

Delete a file and its app goes (prune); change a live object and it comes
back (self-heal).

## Look around

```bash
kubectl --context litek8s-gitops -n argocd get applications
kubectl --context litek8s-gitops -n argocd top pod      # the whole CD controller
litek8s ui                                              # 0: Apps, D: diff with git
```

## Clean up

```bash
litek8s dev delete --name gitops
```

| variable | default | |
|---|---|---|
| `CLUSTER` | `gitops` | dev cluster name; the context is `litek8s-<name>` |
| `REPO_URL`, `REVISION` | this repository, `main` | where `root.yaml` reads `gitops/apps/` |
| `LITECD_VERSION` | the latest release | litecd release to install |
| `LITECD_INSTALL` | litecd's install.sh on its main branch | a URL or a local file |
