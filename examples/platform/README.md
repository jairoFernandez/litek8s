# A self-hosted GitOps platform (Gitea or GitHub + Argo CD)

The stack of [kubefirst](https://github.com/konstructio/kubefirst)'s
platform, built up in steps on your own cluster, with nothing outside it:
[Gitea](https://gitea.com) instead of GitHub (repos, pull requests,
webhooks, a container registry, Actions with GitHub Actions syntax run by
[act_runner](https://gitea.com/gitea/act_runner)), a `gitops` repository in
it, and [Argo CD](https://argo-cd.readthedocs.io) syncing that repository as
an app of apps; or GitHub instead of Gitea (`GIT_PROVIDER=github`). Its layout and components follow kubefirst's
[gitops-template](https://github.com/konstructio/gitops-template) (MIT,
see [gitops/NOTICE.md](gitops/NOTICE.md)); this project is not affiliated
with kubefirst or Konstruct.

| file | what |
|---|---|
| [`up.sh`](up.sh) | installs or updates everything; keeps its secrets and the repository on re-runs |
| [`smoke.sh`](smoke.sh) | CI jobs per runner mode; every Argo CD Application Synced and Healthy on the repository's commit; Argo CD, Gitea and OpenBao over HTTPS with the platform's CA; a secret from OpenBao to a Kubernetes Secret through external-secrets; a two-step Argo Workflow; a push to the sample app served in development; a pull request that Atlantis plans and applies with OpenTofu against OpenBao, then undoes |
| [`lib.sh`](lib.sh) | the Git provider (Gitea over a port-forward, or GitHub): API, git, repositories, Actions secrets and variables; for both |
| [`gitea/`](gitea/) | the Gitea chart's values, act_runner per mode |
| [`gitops/`](gitops/) | the repository's root: `atlantis.yaml`, `terraform/openbao` (OpenBao's policies in OpenTofu), `registry/environments/development` (the sample app) |
| [`metaphor/`](metaphor/) | the sample app's repository: a static page, its Dockerfile and its CI (`.github/workflows/ci.yaml`, for Gitea Actions and GitHub Actions) |
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
| 40 | `cluster-secret-store` | ClusterSecretStore `openbao`: ExternalSecrets in any namespace read OpenBao's KV `secret/`; `openbao-platform` reads `platform/`, for the `atlantis` namespace only |
| 50 | `argo-workflows` | chart 2.0.12 (v4.1.5), workflows in namespace `argo`, UI with token login |
| 50 | `atlantis` | chart 6.16.1 (v0.48.1) on Gitea, OpenTofu, state in Kubernetes Secrets; its credentials from OpenBao |
| 70 | `development` | the sample app `metaphor` in namespace `development` |
| 60 | `reloader` | 2.2.18 |
| 100 | `argocd` | v3.5.5, managing itself |

Argo CD, Gitea, OpenBao, Argo Workflows and Atlantis are at
`https://{argocd,gitea,openbao,argo,atlantis}.<DOMAIN>`, the sample app at
`https://metaphor-development.<DOMAIN>`, with certificates the platform's CA
signs.
kubefirst's ingress-nginx is retired upstream (no releases since March
2026); Traefik takes its place.

Next: staging and production with promotion between them, a registry for
clusters with more than one node, and single sign-on.

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

From a new dev cluster on a Mac (VM with 4 CPUs, 4 GiB): `up.sh` 149 s,
then `smoke.sh` 190 s until everything above passes (image pulls
included). Memory: Argo CD 723 MiB, Gitea and the lkdocker runner 238 MiB,
cert-manager 176 MiB, external-secrets 164 MiB, Atlantis 123 MiB, OpenBao
97 MiB, Argo Workflows 73 MiB, Traefik 52 MiB, reloader 15 MiB; the whole
node 2.0 GiB (kubefirst's local mode asks Docker for 5 GiB, its cloud
clusters 4 x 8 GiB).

`up.sh` takes (the header of [`up.sh`](up.sh) has them all):

| variable | default | |
|---|---|---|
| `GIT_PROVIDER` | `gitea` | `gitea` or `github` ([below](#on-github)) |
| `PLATFORM_MODE` | `lk` | how CI images reach pods ([below](#platform-modes)) |
| `GITOPS_REPO`, `APP_REPO` | `gitops`, `metaphor` | the repositories' names |
| `CLUSTER_NAME` | `mgmt` | the directory under `registry/clusters/` |
| `RUNNERS` | `lkdocker` | Gitea's act_runner modes ([below](#runner-modes-gitea)) |
| `DOMAIN` | `localhost` | the UIs at `https://<app>.<DOMAIN>` |
| `HTTP_PORT`, `HTTPS_PORT` | 80, 443 | the ingress's ports; another one when something on the machine holds them (`HTTPS_PORT=8443`, the URLs then carry it) |

`up.sh` keeps what it chose in the ConfigMap `argocd/platform`, which
`smoke.sh` reads. The gitops repository is `<owner>/gitops` in Gitea
(`lkadmin`) or GitHub; change the platform by pushing to it. `up.sh` writes
it only when it is empty, or with `SEED=update` (a new commit with this
checkout's `gitops/`).

Remove it all: `kubectl delete namespace argocd` first (or Argo CD puts
things back), then the namespaces of the components, then
`helm -n gitea uninstall gitea; kubectl delete namespace gitea`.

## On GitHub

`GIT_PROVIDER=github` keeps the repositories on GitHub instead of Gitea:
no Gitea, no act_runner, and no Atlantis (it needs GitHub's webhooks, which
cannot reach a cluster at home or on a laptop). What changes:

- `up.sh` creates `<owner>/<GITOPS_REPO>` and `<owner>/<APP_REPO>` (private,
  with `gh`) unless they exist, and pushes to them when empty.
- Argo CD reads the gitops repository with the token, and polls it (3
  minutes; there is no webhook into the cluster).
- A self-hosted GitHub Actions runner (`ghcr.io/actions/actions-runner`,
  label `lkdocker`, ephemeral: one job per registration) runs in the cluster
  on litek8s's Docker API, so the sample app's CI works as with Gitea.

It needs the `gh` CLI, logged in (repositories, Actions secrets and
variables), and a fine-grained token for the two repositories with
**Contents**, **Workflows** (to push the app's `.github/workflows`) and
**Administration** (the runner gets its registration tokens with it), all
read and write. The token goes to OpenBao's
`platform/github`, where the runner's ExternalSecret reads it, to Argo CD's
repository Secret, and to the app repository's secret `GITOPS_TOKEN`.

```bash
(umask 077; pbpaste > ~/.github-token)    # the token, not in your shell history
GIT_PROVIDER=github GITOPS_REPO=platform-gitops APP_REPO=platform-app \
  GITHUB_TOKEN_FILE=~/.github-token examples/platform/up.sh
examples/platform/smoke.sh
```

## Platform modes

How the images the CI builds reach the pods:

- `PLATFORM_MODE=lk` (default): the runner builds with litek8s's Docker API,
  into the cluster's own image store; pods start from it with
  `imagePullPolicy: Never`. No registry. It needs that Docker API on the
  node the runner and the app run on: a dev cluster, or a server started
  with `litek8s apiserver --docker-socket`.
- `PLATFORM_MODE=dind` (not yet): the runner builds in its own dockerd and
  pushes to a registry (Gitea's), and nodes pull from it. That needs nodes
  that trust the platform's CA and log in to the registry, which litek8s's
  registry configuration is about to give them.

## The sample app: push to deploy

`up.sh` puts [`metaphor/`](metaphor/) in the provider as
`<owner>/<APP_REPO>` with the Actions secret `GITOPS_TOKEN` (a token that
can write to the gitops repository) and the variable `GITOPS_REPO`. Every
push to its `main`:

1. Gitea Actions or GitHub Actions runs `.github/workflows/ci.yaml` on a
   runner in the cluster with the label `lkdocker`. With Gitea the runner
   mounts its Docker socket, litek8s's Docker API, into the job; the GitHub
   runner uses it itself. `docker build` puts `metaphor:<commit>` straight
   into the cluster's image store, the one pods start from: no registry,
   nothing pushed or pulled.
2. The job sets `newTag: <commit>` in the gitops repository's
   `registry/environments/development/metaphor/kustomization.yaml`.
3. Gitea's webhook tells Argo CD about the push (on GitHub Argo CD polls,
   up to 3 minutes), and Argo CD rolls out the new image (`imagePullPolicy: Never`, on the node with
   the Docker engine).

`smoke.sh` changes the page with a commit and waits for
`https://metaphor-development.<DOMAIN>` to serve it. Each build leaves its
image in the store (`docker image prune` on the cluster's Docker API).
`SEED=update` leaves `registry/environments` in the repository as it is:
the tags there are the CI's.

## Infrastructure as code: Atlantis and OpenTofu

The repository's `terraform/openbao` holds OpenBao's policies (`admin`,
`developer`) in OpenTofu. Change them in a pull request on Gitea: Atlantis
(the user `atlantis`, write access to the repository, webhook from it)
comments the plan; comment `atlantis apply` to apply, then `atlantis unlock`,
then merge (Atlantis v0.48 reads Gitea's "closed" event, sent on close and
on merge, as "other" and keeps the project locked otherwise; the lock can
also be deleted in Atlantis's UI). The state
is a Secret in the namespace `atlantis` (OpenTofu's `kubernetes` backend),
so there is no bucket to run. Atlantis reaches OpenBao with the root token
(it manages policies), its Gitea token and webhook secret: `up.sh` puts the
three in OpenBao's `platform/atlantis`, and an ExternalSecret through the
store `openbao-platform` (limited to the namespace `atlantis`) makes them
its Secret. kubefirst does the same with Terraform, Vault and GitHub.

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

## Runner modes (Gitea)

| mode | job containers | needs |
|---|---|---|
| `lkdocker` | litek8s's own Docker API: crun containers, no dockerd, no privileged pod | a node with the litek8s Docker engine (a dev cluster's server, `apiserver --docker-socket`) |
| `dind` | a real dockerd in a privileged sidecar (`docker:27-dind`, ~200 MiB); litek8s only runs the pod | any node |
| `host` | none: steps run in the runner pod (Alpine), no isolation | any node |

`RUNNERS="lkdocker dind host"` installs all three; `smoke.sh lkdocker dind host` checks them.

## Notes

- **Client-side apply in Argo CD.** Argo CD's diff for server-side apply
  needs the server's OpenAPI models of the built-in types, which litek8s
  0.2.1 and older do not serve (`unable to resolve parseableType for
  GroupVersionKind: /v1, Kind=ServiceAccount`; served since, see
  CHANGELOG.md). So that the example runs on those too, the Applications use
  client-side apply, and the three CRDs too big for it (its 256 KiB
  annotation: external-secrets' SecretStore and ClusterSecretStore, Argo
  CD's ApplicationSet) carry `argocd.argoproj.io/sync-options: Replace=true`.
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
- **Gitea's URLs**: `PUBLIC_URL_DETECTION: never`, so webhook payloads and
  links always carry `ROOT_URL`; with Gitea's default (`auto`, the request's
  Host) a pull request opened from a browser would send Atlantis a clone URL
  it cannot reach.
- **Atlantis after an undo**: a pull request that adds and then removes a
  file no longer differs from main, so nothing is autoplanned; comment
  `atlantis plan` to plan the removal.
- Runners keep their registration in an `emptyDir`: a new pod registers again
  and Gitea lists the old one as offline.
