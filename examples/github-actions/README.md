# litek8s in GitHub Actions

Workflows that test on litek8s instead of kind or the runner's dockerd,
with the [`setup-litek8s`](../../.github/actions/setup-litek8s/action.yml)
action. Copy one into your repository's `.github/workflows/` and adjust
the paths. Measurements against kind and dockerd: [docs/ci.md](../../docs/ci.md).

| example | what it shows | replaces |
|---|---|---|
| [helm/](helm/) | `helm install --wait`, `helm test`, upgrade and rollback of a chart | `helm/kind-action` |
| [compose/](compose/) | `docker compose up --build --wait`, tests from the runner and from a container | the runner's dockerd |
| [multi-node/](multi-node/) | 3 nodes: topology spread, a DaemonSet, Service traffic across nodes, draining a node | kind with workers |
| [build-deploy/](build-deploy/) | `docker build`, deploy, smoke test, then a second version: the build is in the cluster's image store at once | kind or k3d plus `kind load` / `k3d image import` |

```yaml
- uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
  with:
    nodes: 1            # control plane + N-1 workers
    # version: v0.2.1   # default: the latest release
    # docker: true      # export DOCKER_HOST (litek8s's Docker API)
```

After it, `kubectl`, `helm` and `docker` (all preinstalled on GitHub's
Ubuntu runners) talk to the cluster; the action's outputs are `context`,
`kubeconfig`, `api-url`, `docker-socket`, `docker-host` and
`ready-seconds`. Linux runners only (`ubuntu-*`, x86_64 and arm64).

This repository's CI runs the helm, compose and multi-node examples on
each commit (`examples` job in
[test.yml](../../.github/workflows/test.yml)); build-deploy runs in
[ci-bench.yml](../../.github/workflows/ci-bench.yml):
[run.sh](run.sh) executes the `run:` steps of an example's `workflow.yml`
in order from its directory, after the local action set up the cluster.
