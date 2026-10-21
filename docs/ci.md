# litek8s in CI

A CI job that needs a Kubernetes cluster usually starts kind (a Docker
container per node, ~45 s on GitHub's runners before the nodes are Ready);
one that needs Docker uses the runner's dockerd. litek8s gives both in
about 2.5 s, download included, as the user running the job: `litek8s dev
up` is the same rootless sandbox as on a laptop (API server, controllers,
scheduler, cluster DNS, node agents on crun, a Docker Engine API), so
kubectl, Helm, `docker`, `docker compose` and Testcontainers work
unchanged.

## GitHub Actions

```yaml
jobs:
  e2e:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@main
      - run: |
          helm install hello ./chart --wait
          helm test hello
```

The action ([action.yml](../.github/actions/setup-litek8s/action.yml))
downloads the release for the runner's architecture and checks it against
the release's SHA256SUMS (with `install.sh`), installs what the rootless
cluster needs if the image lacks it (crun, uidmap, slirp4netns; GitHub's
Ubuntu images have them), runs `sudo litek8s dev setup` for Ubuntu's
AppArmor restriction on user namespaces, starts the cluster and waits for
every node to be Ready. Later steps get `KUBECONFIG` and, unless
`docker: false`, `DOCKER_HOST` (plus `DOCKER_BUILDKIT=0` and
`COMPOSE_BAKE=false`, so builds use litek8s's own builder). If a step of the
action fails it prints `litek8s dev status`, pods, events and the cluster's
logs; `litek8s-diagnose` (on the PATH,
[diagnose.sh](../.github/actions/setup-litek8s/diagnose.sh)) does the same
for your own steps:

```yaml
      - if: failure()
        run: litek8s-diagnose
```

| input | default | |
|---|---|---|
| `version` | `latest` | a release tag, e.g. `v0.2.1` |
| `binary` | | a litek8s binary to use instead (built earlier in the job) |
| `cluster-name` | `ci` | kube context `litek8s-<name>` |
| `nodes` | `1` | control plane + N-1 workers, joined like real nodes |
| `docker` | `true` | export `DOCKER_HOST` for docker, compose, Testcontainers |
| `builder` | `litek8s` | `buildkit` to build through buildx's BuildKit container instead |
| `network` | `slirp4netns` | pods' egress; `pasta` is litek8s's desktop default (see below) |
| `publish` | `localhost` | `all` to publish NodePorts and ports on every address |
| `wait` | `120` | seconds to wait for the nodes |

Outputs: `version`, `context`, `kubeconfig`, `api-url`, `docker-socket`,
`docker-host`, `ready-seconds`. Linux runners only (`ubuntu-*`, x86_64 and
arm64): on macOS the dev cluster is a VM, and GitHub's macOS runners allow
no nested virtualization.

Pin the action to a release tag (`@v0.2.1` and later) once one includes it;
`@main` follows the repository.

### Examples

[examples/github-actions/](../examples/github-actions/README.md) has three
workflows to copy, each run on every commit of this repository (the
`examples` job of [test.yml](../.github/workflows/test.yml), through
[run.sh](../examples/github-actions/run.sh), which executes an example's
`run:` steps in order):

- **[helm](../examples/github-actions/helm/workflow.yml)**: `helm lint`,
  `helm install --wait`, `helm test`, upgrade and rollback of a small chart
  (a Deployment behind a Service with a readiness probe, a ConfigMap, a test
  pod reaching the Service by DNS). Replaces `helm/kind-action`.
- **[compose](../examples/github-actions/compose/workflow.yml)**:
  `docker compose up --build --wait` of a Python app (built from its
  Dockerfile) and Redis with healthchecks, tested from the runner through
  the published port and from a container on the project network.
  Replaces the runner's dockerd.
- **[multi-node](../examples/github-actions/multi-node/workflow.yml)**: three
  nodes; a Deployment spread one replica per node by a topology spread
  constraint, a DaemonSet, a Service reached from every node, a drained
  worker's pod leaving it.

## Numbers

The same examples on the same kind of runner (GitHub's `ubuntu-24.04`,
4 vCPU, 16 GB), with litek8s v0.2.0 from its release, against kind
(`helm/kind-action@v1`, kind v0.33.0, its default node image) and against
the runner's own dockerd; [ci-bench.yml](../.github/workflows/ci-bench.yml),
run by hand. Medians of 5 runs ([run 39559413930](https://github.com/jairoFernandez/litek8s/actions/runs/39559413930), attempts 1 to 5); every job starts cold (no cache: the
images are pulled from Docker Hub in every job, by every engine).

| example | engine | time to ready | example steps | job total | memory idle | memory peak |
|---|---|---|---|---|---|---|
| helm | **litek8s** | **2.6 s** | **10.2 s** | **21 s** | **12 MiB** | **42 MiB** |
| helm | kind | 44.7 s | 22.7 s | 79 s | 449 MiB | 549 MiB |
| multi-node (3 nodes) | **litek8s** | **2.5 s** | **4.5 s** | **15 s** | **28 MiB** | **82 MiB** |
| multi-node (3 nodes) | kind | 49.7 s | 11.1 s | 72 s | 715 MiB | 827 MiB |
| compose | **litek8s** | 2.2 s | **21.0 s** | **31 s** | 13 MiB | 46 MiB |
| compose | runner's dockerd | **0 s** (already running) | 34.0 s | 44 s | 0 MiB | **28 MiB** |

Ranges over the 5 runs: litek8s time to ready 2.1 to 4.1 s, kind 40.8 to
53.2 s; job totals litek8s 13 to 38 s, kind 67 to 85 s, dockerd 37 to 49 s.

- A Kubernetes job is **3.7 to 4.8 times shorter** (median job totals), and
  the cluster takes **1/37 (helm) to 1/26 (multi-node) of kind's memory**:
  kind runs a whole node (systemd, containerd, kubelet, etcd, the API
  server, kube-proxy, CoreDNS, kindnet) per container.
- The example steps themselves are faster too (helm 10.2 s against
  22.7 s, multi-node 4.5 s against 11.1 s); these runs do not break down
  where kind's time goes (image pulls into the nodes, pod start, Service
  rules).
- Against the runner's dockerd, compose builds and starts faster on
  litek8s (litek8s's builder against BuildKit's) and the whole job is 13 s
  shorter, though dockerd needs no setup at all. Its containers take less
  memory than litek8s's whole sandbox, which also holds a Kubernetes API
  server and node; dockerd and containerd themselves are not counted.
- Not compared: the runner's preinstalled kind (`kind create cluster`
  without the action) and k3d; a cluster image cached between runs. The
  litek8s numbers are for v0.2.0 with the action's defaults
  (slirp4netns).

What the columns measure:

- **Time to ready**: from the start of the setup step to every node
  Ready: download and checks included for litek8s, kind's node image pull
  and cluster creation for kind (whose action waits for the control plane;
  the job then waits for the nodes, as litek8s's action does). dockerd is
  already running on the runner, so it has nothing to wait for.
- **Example steps**: the example's own `run:` steps (image pulls, Helm, the
  tests).
- **Job total**: the job as GitHub times it, checkout and the measuring
  steps included.
- **Memory idle / peak**: cgroup `memory.current - inactive_file` (what
  `docker stats` shows), sampled every 200 ms from the moment the cluster
  is ready to the end of the example: for litek8s its whole sandbox (API
  server, controllers, node agents, slirp4netns, pods and Docker API
  containers), for kind and dockerd every Docker container (kind's node
  containers with everything in them; compose's services). The runner's
  dockerd and containerd run in every case and are not counted.

Caching the image store between runs (`actions/cache`) is not offered:
the unpacked layers belong to the cluster's subordinate user ids, which the
runner user cannot read back into a cache archive, and pulls from Docker
Hub took 0.7 to 6 s per image on these runners.

For this repository's own CI, see [CI of litek8s itself](#ci-of-litek8s-itself).

## Other CI systems

On any Linux machine or VM a job controls (GitLab CI with a shell or
privileged Docker executor, Jenkins agents, Buildkite, CircleCI's machine
executor), [examples/ci/litek8s-ci.sh](../examples/ci/litek8s-ci.sh) does
what the action does and prints the environment to export:

```bash
eval "$(sh examples/ci/litek8s-ci.sh)"
kubectl get nodes
docker compose up -d --wait
```

Or by hand:

```bash
sudo apt-get install -y crun uidmap slirp4netns
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
sudo "$(command -v litek8s)" dev setup     # Ubuntu >= 23.10: AppArmor allows litek8s user namespaces
LITEK8S_DEV_NETWORK=slirp4netns litek8s dev up --name ci
export DOCKER_HOST="unix://$HOME/.litek8s/ci/docker.sock"
```

GitLab CI (GitLab.com's Linux runners run jobs in privileged containers,
which `docker:dind` needs too; as root the cluster runs without a user
namespace). Not run on GitLab itself: the same container setup is tested
on GitHub's runner, below.

```yaml
k8s-e2e:
  image: ubuntu:24.04
  script:
    - apt-get update -qq && apt-get install -y -qq curl ca-certificates
    - eval "$(sh examples/ci/litek8s-ci.sh)"
    - curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/v1.33.0/bin/linux/amd64/kubectl" && chmod +x /usr/local/bin/kubectl
    - kubectl apply -f deploy/ && kubectl rollout status deployment/myapp --timeout=2m
```

`litek8s-ci.sh` is tested in [ci-bench.yml](../.github/workflows/ci-bench.yml)
both ways on GitHub's Ubuntu runner: as the runner user with sudo, and as
root in a privileged `ubuntu:24.04` container. In 5 runs each the cluster
was up and an nginx Deployment rolled out in a median job time of 26 s
(user) and 28 s (container, apt included). In a container `$HOME` sits on
the container's overlayfs, which cannot hold litek8s's own overlay mounts
(as with Docker in Docker); the script puts litek8s's data on a tmpfs
there.

## pasta, slirp4netns and GitHub's runners

Pods (and the node agent pulling images) reach the Internet through a
userspace network stack: pasta when it is installed, else slirp4netns. On
GitHub's Ubuntu runners pasta (Ubuntu 24.04's `passt 2024-02-20`, and the
2026-10-02 static build as well) drops TCP flows now and then: the
connection looks established inside the cluster, the server never sees the
data. Of 5 runs each that started a cluster, pulled two images and started a
pod, 3 hung with Ubuntu's pasta, 1 with the latest pasta and none with
slirp4netns; with the registry client fixed below (three images per run),
pasta still failed 2 of 5, slirp4netns none, and its pulls were as fast
(0.7 to 5.9 s per image against 1.7 to 5.0 s). So:

- the action uses slirp4netns (`network: slirp4netns`, which sets
  `LITEK8S_DEV_NETWORK=slirp4netns`; litek8s 0.2.0 does not read that
  variable yet, so for it the action moves `/usr/bin/pasta` aside on the
  runner);
- litek8s's registry client now gives each address of a registry 10 s to
  connect and 15 s for the TLS handshake before it tries the next one, and
  reads small responses within a minute: before, a dropped flow left the
  pull, and the pod in `ContainerCreating`, hanging for good.

On desktops pasta stays the default; `LITEK8S_DEV_NETWORK=slirp4netns
litek8s dev up` picks slirp4netns there too.

## CI of litek8s itself

[test.yml](../.github/workflows/test.yml) on every push and pull request:

| job | what |
|---|---|
| `cargo test (ubuntu-latest)`, `(macos-latest)` | every unit and integration test with [cargo-nextest](https://nexte.st) |
| `build-linux` | the commit's binary for the jobs below |
| `compat` | the compatibility corpus's API tiers (L0 discovery, L1 objects and CRDs, 50 cases) against `litek8s apiserver --no-node` on the runner: no Docker, no VM |
| `example (helm\|compose\|multi-node)` | the GitHub Actions examples on the commit's binary through the local action |

What made it faster (several runs each, warm caches):

| | before (cargo test) | after (cargo nextest, no debug info) |
|---|---|---|
| `cargo test (ubuntu-latest)` job | 171 s (158 to 213) | **98 s** (95 to 156) |
| its test step (build + run) | 158 s | **85 s** |
| `cargo test (macos-latest)` job | 293 s (237 to 371) | **218 s** (105 to 250) |
| its test step (build + run) | 267 s | **187 s** |
| workflow, first job start to last job end | 298 s (240 to 378) | **224 s** (109 to 257), with the compat and example jobs added |
| test execution alone (ubuntu) | 65 s | 41 to 47 s |

Before: main's test.yml of 690a2d5 rerun on that commit with main's warm
cache, 4 times ([run 39557051657](https://github.com/jairoFernandez/litek8s/actions/runs/39557051657));
after: this test.yml, 5 attempts of [run 39559413919](https://github.com/jairoFernandez/litek8s/actions/runs/39559413919),
warm branch cache. Medians, ranges in parentheses. GitHub's macOS runners
vary the most (105 to 250 s for the same job). The new jobs run beside the
macOS one: `build-linux` 66 s, then `compat` 52 s and the examples 13 to
33 s each.

- **No debug info** (`CARGO_PROFILE_DEV_DEBUG=0`): the crate and its tests
  compile and link faster, and the cache is smaller (Linux 268 MiB
  instead of 346, macOS 293 instead of 314).
  Backtraces keep function names, not line numbers.
- **cargo-nextest**: every test in its own process, test binaries side by
  side. `tests/ha.rs` alone took 38 s of `cargo test`'s 65 s, run after the
  400 unit tests; now the rest runs beside it. `.config/nextest.toml` keeps
  the HA tests one at a time, as `cargo test` did, and stops a hung test
  after 5 minutes. There are no doctests, so nothing is lost.
- **build.rs reran on every build in CI**: it asked cargo to watch
  `.git/packed-refs`, which a fresh checkout lacks, and cargo reruns a build
  script (and rebuilds the crate) for a watched file that does not exist.
  Harmless with one `cargo test`, ~50 s with a build step and a test step.
- **The cache**: the last runs on main said "No cache found" because 0.2.0
  changed the job matrix, so every cache key; nothing was wrong with the
  keys. The cache is now saved from main only (branches and pull requests
  restore main's).

Not done: merging the four `tests/*.rs` into one test binary (three fewer
links of the crate). With nextest running them side by side and no debug
info, it was not worth losing `cargo test --test ha` and the per-area files.

`cargo test` still runs every test locally; `cargo nextest run` matches CI.
