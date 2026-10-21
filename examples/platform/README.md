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
| [`smoke.sh`](smoke.sh) | CI jobs per runner mode; every Argo CD Application Synced and Healthy on the repository's commit; Argo CD, Gitea and OpenBao over HTTPS with the platform's CA; a secret from OpenBao to a Kubernetes Secret through external-secrets |
| [`lib.sh`](lib.sh) | Gitea API and git over a port-forward, for both |
| [`gitea/`](gitea/) | the Gitea chart's values, act_runner per mode |
| [`gitops/cluster/`](gitops/cluster/) | what goes to `registry/clusters/<cluster>/` in the repository: one Argo CD Application per component, `registry.yaml` the app of apps, `components/argocd` Argo CD itself |

What is in it so far:

| wave | Application | what |
|---|---|---|
| 0 | `registry` | the app of apps (this directory, itself included) |
| 10 | `cert-manager` | v1.21.2 |
| 10 | `traefik` | chart 41.7.1 (Traefik v3.7), the default IngressClass, a LoadBalancer on `HTTP_PORT`/`HTTPS_PORT` |
| 20 | `cert-issuers` | the platform's own CA (`platform-ca`, self-signed root) and its ClusterIssuer |
| 20 | `openbao` | chart 0.30.3 (OpenBao v2.7.1, the open-source fork of Vault): standalone, unseals itself with a static key |
| 30 | `external-secrets-operator` | 2.13.0 |
| 40 | `cluster-secret-store` | ClusterSecretStore `openbao`: ExternalSecrets in any namespace read OpenBao's KV `secret/` |
| 60 | `reloader` | 2.2.18 |
| 100 | `argocd` | v3.5.5, managing itself |

Argo CD, Gitea and OpenBao are at `https://argocd.<DOMAIN>`,
`https://gitea.<DOMAIN>` and `https://openbao.<DOMAIN>`, with certificates
the platform's CA signs.
kubefirst's ingress-nginx is retired upstream (no releases since March
2026); Traefik takes its place.

Next: Argo Workflows, Atlantis with OpenTofu on Gitea (OpenBao's policies,
auth and users as code, as kubefirst does with Terraform), the sample app's
CI/CD across environments.

## On a dev cluster

```bash
litek8s dev up
examples/platform/up.sh
examples/platform/smoke.sh

open https://argocd.localhost   # user admin
kubectl -n argocd get secret argocd-initial-admin-secret -o jsonpath='{.data.password}' | base64 -d
open https://gitea.localhost    # user lkadmin
kubectl -n gitea get secret gitea-admin -o jsonpath='{.data.password}' | base64 -d
open https://openbao.localhost  # root token
kubectl -n openbao get secret openbao-init -o jsonpath='{.data.root-token}' | base64 -d
```

Browsers, curl and most tools resolve `*.localhost` to this machine, so the
default `DOMAIN=localhost` needs no DNS; a dev cluster publishes the
ingress's LoadBalancer ports on `127.0.0.1`. To trust the platform's CA
(and drop the browser warning), add it to your keychain or browser:

```bash
kubectl -n cert-manager get secret platform-ca -o jsonpath='{.data.ca\.crt}' | base64 -d > platform-ca.crt
```

On real machines, set `DOMAIN` to a name whose `*.` records point at a node
(a home DNS server or router), and the ports stay 80/443.

From a new dev cluster on a Mac (VM with 4 CPUs, 4 GiB): `up.sh` 1 min 47 s,
then `smoke.sh` 1 min 56 s until everything above passes (image pulls
included). Memory: Argo CD 552 MiB, Gitea and the lkdocker runner 215 MiB,
external-secrets 126 MiB, cert-manager 119 MiB, OpenBao 89 MiB, Traefik
46 MiB, reloader 15 MiB; the whole node 1.5 GiB.

`up.sh` takes `CLUSTER_NAME` (default `mgmt`, the directory in the
repository), `RUNNERS` (default `lkdocker`), `DOMAIN` (default `localhost`)
and `HTTP_PORT`/`HTTPS_PORT` (default 80/443; another port when something
on the machine holds them, e.g. `HTTPS_PORT=8443`, then the URLs carry it). The repository is
`lkadmin/gitops` in Gitea; change the platform by pushing to it. `up.sh`
writes it only when it is empty, or with `SEED=update` (a new commit with
this checkout's `gitops/`).

Remove it all: `kubectl delete namespace argocd` first (or Argo CD puts
things back), then the namespaces of the components, then
`helm -n gitea uninstall gitea; kubectl delete namespace gitea`.

## Secrets

OpenBao keeps its data on a 1 GiB volume and seals it with a static key
(`seal "static"`, OpenBao 2.5+): `up.sh` puts 32 random bytes in the Secret
`openbao/openbao-unseal-key` before Argo CD deploys it, and OpenBao unseals
itself with them on every start, no unseal step and no KMS. `up.sh` then
does what kubefirst's API does for Vault: initializes it once (root token and
recovery key in the Secret `openbao/openbao-init`), enables the KV v2 store
`secret/` and Kubernetes auth with a read-only role for external-secrets.

Use it from any namespace:

```yaml
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata: {name: db, namespace: myapp}
spec:
  secretStoreRef: {kind: ClusterSecretStore, name: openbao}
  target: {name: db}
  data:
    - secretKey: password
      remoteRef: {key: myapp/db, property: password}   # bao kv put secret/myapp/db password=...
```

Whoever can read Secrets in the `openbao` namespace can unseal and act as
root: fine for a home lab or a dev cluster (kubefirst keeps Vault's unseal
keys in a Secret too); with a cloud KMS or an HSM, replace the `static`
seal in `gitops/cluster/openbao.yaml`.

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
- **Gitea's own links** (clone URLs in its UI) show the in-cluster address:
  runners and job containers clone from Gitea's `ROOT_URL`, which must be
  reachable from inside the cluster, where `*.localhost` is the pod itself.
- Runners keep their registration in an `emptyDir`: a new pod registers again
  and Gitea lists the old one as offline.
