# A self-hosted GitOps platform (Gitea + Argo CD)

The stack of [kubefirst](https://github.com/konstructio/kubefirst)'s
platform, built up in steps on your own cluster, with nothing outside it:
[Gitea](https://gitea.com) instead of GitHub (repos, pull requests,
webhooks, a container registry, Actions with GitHub Actions syntax run by
[act_runner](https://gitea.com/gitea/act_runner)), a `gitops` repository in
it, and [Argo CD](https://argo-cd.readthedocs.io) syncing that repository as
an app of apps. Its layout and components follow kubefirst's
[gitops-template](https://github.com/konstructio/gitops-template) (MIT,
see [gitops/NOTICE.md](gitops/NOTICE.md)); this project is not affiliated
with kubefirst or Konstruct.

| file | what |
|---|---|
| [`up.sh`](up.sh) | installs or updates everything; keeps its secrets and the repository on re-runs |
| [`smoke.sh`](smoke.sh) | CI jobs per runner mode, then every Argo CD Application Synced and Healthy |
| [`lib.sh`](lib.sh) | Gitea API and git over a port-forward, for both |
| [`gitea/`](gitea/) | the Gitea chart's values, act_runner per mode |
| [`gitops/cluster/`](gitops/cluster/) | what goes to `registry/clusters/<cluster>/` in the repository: one Argo CD Application per component, `registry.yaml` the app of apps, `components/argocd` Argo CD itself |

What is in it so far:

| wave | Application | what |
|---|---|---|
| 0 | `registry` | the app of apps (this directory, itself included) |
| 10 | `cert-manager` | v1.21.2 |
| 10 | `ingress-nginx` | 4.15.1 (the project is retired; to be replaced) |
| 30 | `external-secrets-operator` | 2.13.0 |
| 60 | `reloader` | 2.2.18 |
| 100 | `argocd` | v3.5.5, managing itself |

Next: TLS and ingress with a local CA and domain, OpenBao (secrets, with
external-secrets), Argo Workflows and Atlantis on Gitea, the sample app's
CI/CD across environments.

## On a dev cluster

```bash
litek8s dev up
examples/platform/up.sh
examples/platform/smoke.sh

kubectl -n gitea port-forward svc/gitea-http 3000:3000     # http://127.0.0.1:3000, user lkadmin
kubectl -n gitea get secret gitea-admin -o jsonpath='{.data.password}' | base64 -d
kubectl -n argocd port-forward svc/argocd-server 8080:80   # http://127.0.0.1:8080, user admin
kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d
```

From a new dev cluster on a Mac (VM with 4 CPUs, 4 GiB): `up.sh` 90 s,
then `smoke.sh` 2 min 40 s until every Application is Synced and Healthy
(image pulls included). Memory: Argo CD 585 MiB, Gitea and the lkdocker
runner 209 MiB, external-secrets 139 MiB, cert-manager 122 MiB,
ingress-nginx 104 MiB, reloader 12 MiB; the whole node 1.4 GiB.

`up.sh` takes `CLUSTER_NAME` (default `mgmt`, the directory in the
repository) and `RUNNERS` (default `lkdocker`). The repository is
`lkadmin/gitops` in Gitea; change the platform by pushing to it. `up.sh`
writes it only when it is empty, or with `SEED=update` (a new commit with
this checkout's `gitops/`).

Remove it all: `kubectl delete namespace argocd` first (or Argo CD puts
things back), then the namespaces of the components, then
`helm -n gitea uninstall gitea; kubectl delete namespace gitea`.

## Runner modes

| mode | job containers | needs |
|---|---|---|
| `lkdocker` | litek8s's own Docker API: crun containers, no dockerd, no privileged pod | a node with the litek8s Docker engine (a dev cluster's server, `apiserver --docker-socket`) |
| `dind` | a real dockerd in a privileged sidecar (`docker:27-dind`, ~200 MiB); litek8s only runs the pod | any node |
| `host` | none: steps run in the runner pod (Alpine), no isolation | any node |

`RUNNERS="lkdocker dind host"` installs all three; `smoke.sh lkdocker dind host` checks them.

## Notes

- **No ServerSideApply in Argo CD (yet).** Argo CD's diff for server-side
  apply needs the server's OpenAPI models of the built-in types, which
  litek8s does not serve yet (`unable to resolve parseableType for
  GroupVersionKind: /v1, Kind=ServiceAccount`). The Applications use
  client-side apply; the three CRDs too big for it (its 256 KiB annotation:
  external-secrets' SecretStore and ClusterSecretStore, Argo CD's
  ApplicationSet) carry `argocd.argoproj.io/sync-options: Replace=true`.
- **No wait Jobs.** kubefirst orders its waves with Jobs that wait for
  Deployments; here Argo CD's health check for Applications (in
  `components/argocd/argocd-cm.yaml`) makes the app of apps wait for each
  wave instead.
- **Gitea chart defaults.** Its `RollingUpdate` starts the new pod while the
  old one holds the leveldb queue lock (crash loop) and its `ROOT_URL`
  (`git.example.com`) breaks `actions/checkout`; `gitea/values.yaml` fixes
  both.
- Runners keep their registration in an `emptyDir`: a new pod registers again
  and Gitea lists the old one as offline.
