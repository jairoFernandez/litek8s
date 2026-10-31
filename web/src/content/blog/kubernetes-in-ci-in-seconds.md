---
title: "Kubernetes in CI in ~2.5 s instead of ~45 s: litek8s against kind"
description: "A one-line change in a GitHub Actions workflow: a cluster Ready in ~2.5 s instead of ~45 s, Helm jobs 3.7x shorter. Measured, with where it does not pay off."
date: 2026-10-30
tags: [ci, github actions, gitlab, kind, helm, performance]
docs: [ci, numbers, dev, limitations]
translation: es/kubernetes-en-ci-en-segundos
---

Most Kubernetes e2e jobs start the same way: `helm/kind-action`, and then
a wait. kind runs each node as a Docker container with a whole node inside
(systemd, containerd, kubelet, etcd, the API server, kube-proxy, CoreDNS,
kindnet), and on GitHub's `ubuntu-24.04` runners that took a median of
**44.7 s** before the nodes were Ready. Every push, every pull request,
before a single test runs.

With litek8s the same cluster is Ready in **2.6 s**, download of the
binary included. This post is about that number: how we measured it, the
one line that changes in a workflow, and the cases where switching is not
worth it.

## The diff

This is a Helm e2e workflow. The only change is the setup step:

```diff
     steps:
       - uses: actions/checkout@v4
-      - uses: helm/kind-action@v1
+      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
       - run: |
           helm install hello ./chart --wait
           helm test hello
```

The chart, the manifests, `kubectl` and `helm` stay as they are. The
action downloads the release for the runner's architecture, checks it
against the release's `SHA256SUMS`, runs `sudo litek8s dev setup` for
Ubuntu's AppArmor restriction on user namespaces, starts the cluster as
the runner user and waits for every node to be Ready. Later steps get
`KUBECONFIG`, and also `DOCKER_HOST`, because the same cluster serves the
Docker Engine API: `docker`, `docker compose` and Testcontainers work in
the same job.

Three nodes instead of one is an input:

```yaml
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
        with:
          nodes: 3      # control plane + 2 workers, joined like real nodes
```

And when something fails, the action prints `litek8s dev status`, pods,
events and the cluster's logs. For your own steps:

```yaml
      - if: failure()
        run: litek8s-diagnose
```

## The numbers

Three example workflows (a Helm chart with install, test, upgrade and
rollback; three nodes with topology spread, a DaemonSet and a drain; a
compose stack built from a Dockerfile), run on GitHub's `ubuntu-24.04`
runner (4 vCPU, 16 GB) with litek8s v0.2.0 against `helm/kind-action@v1`
(kind v0.33.0, its default node image) and against the runner's own
dockerd. Medians of 5 runs, every job cold: no cache, images pulled from
Docker Hub in every job by every engine.

| example | engine | time to ready | job total | memory idle | memory peak |
|---|---|---|---|---|---|
| helm | **litek8s** | **2.6 s** | **21 s** | **12 MiB** | **42 MiB** |
| helm | kind | 44.7 s | 79 s | 449 MiB | 549 MiB |
| multi-node (3 nodes) | **litek8s** | **2.5 s** | **15 s** | **28 MiB** | **82 MiB** |
| multi-node (3 nodes) | kind | 49.7 s | 72 s | 715 MiB | 827 MiB |
| compose | litek8s | 2.2 s | **31 s** | 13 MiB | 46 MiB |
| compose | runner's dockerd | **0 s** (already running) | 44 s | 0 MiB | **28 MiB** |

Over the 5 runs litek8s was ready in 2.1 to 4.1 s and kind in 40.8 to
53.2 s. Job totals: litek8s 13 to 38 s, kind 67 to 85 s.

Read plainly:

- A Kubernetes job is **3.7 to 4.8 times shorter**, median to median.
- The cluster takes **1/37 (helm) to 1/26 (multi-node) of kind's memory**.
- The example steps themselves are faster too (helm 10.2 s against
  22.7 s, multi-node 4.5 s against 11.1 s). These runs do not break down
  where kind spends that time, so we will not guess.

The workflow that produced this is in the repository
([ci-bench.yml](https://github.com/jairoFernandez/litek8s/blob/main/.github/workflows/ci-bench.yml)),
and so is [the run](https://github.com/jairoFernandez/litek8s/actions/runs/39559413930).
Run it on your own fork before trusting a blog post.

## Where the 42 seconds go

kind's 45 s are honest work: pulling a node image, booting systemd and
containerd inside a container, starting etcd and the control plane, and
then waiting for the node to report Ready.

`litek8s dev up` has nothing to boot. It forks a rootless sandbox (user,
network, mount, PID and cgroup namespaces) and starts the API server,
controllers, scheduler, cluster DNS and a node agent on crun in one
process, with no etcd. It is the same command people run on a laptop,
where it reaches a Ready node in 0.17 s on Linux
([Numbers](/docs/numbers/)). On the runner most of the 2.5 s is the
download and the checks.

## GitLab and Docker in Docker

GitLab.com's Linux runners run jobs in privileged containers, which is
also what `docker:dind` needs. We did not run this on GitLab itself; we
reproduced the setup on GitHub's runner: a privileged container,
`docker:27-dind` plus kind v0.33.0 on one side, and
[litek8s-ci.sh](https://github.com/jairoFernandez/litek8s/blob/main/examples/ci/litek8s-ci.sh)
in `ubuntu:24.04` on the other, its apt packages and download counted in
the time to ready. litek8s v0.2.1, medians of 5 runs (kind in dind: 4, one
`helm test` pod failed inside kind).

| example | engine | time to ready | job total | container memory peak |
|---|---|---|---|---|
| helm | **litek8s** | **9.2 s** | **39 s** | **356 MiB** |
| helm | kind in dind | 40.0 s | 70 s | 2,960 MiB |
| compose | litek8s | 5.3 s | 40 s | 353 MiB |
| compose | dind | **1.0 s** | **38 s** | 345 MiB |

With Kubernetes, ready 4× sooner and the Helm job 1.8× shorter: kind in
dind pulls its ~1 GB node image and runs a whole node inside the job's
container. The GitLab job:

```yaml
k8s-e2e:
  image: ubuntu:24.04
  script:
    - apt-get update -qq && apt-get install -y -qq curl ca-certificates
    - eval "$(sh examples/ci/litek8s-ci.sh)"
    - curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/v1.33.0/bin/linux/amd64/kubectl" && chmod +x /usr/local/bin/kubectl
    - kubectl apply -f deploy/ && kubectl rollout status deployment/myapp --timeout=2m
```

The same script works on any Linux machine or VM a job controls (Jenkins
agents, Buildkite, CircleCI's machine executor): it prints the
environment to export.

## When it does not pay off

The look-at-this-number version of this post would stop here. The useful
one has this table:

| the job needs | instead of | verdict |
|---|---|---|
| a Kubernetes cluster on GitHub's runners | kind, k3d | **switch**: 3–5× faster jobs, 26–37× less memory |
| a Kubernetes cluster in a privileged container | kind in dind | **switch**: Helm job 39 s instead of 70 s |
| Docker only (build, compose, Testcontainers) on GitHub's runners | the runner's dockerd | small gain: 31 s against 44 s for the compose job, and dockerd needs no setup |
| Docker only in a privileged container | Docker in Docker | **no gain**: 40 s against 38 s; dind's dockerd starts in ~1 s |

If your pipeline only builds images and runs compose, keep what you have,
or switch for the smaller gain if you already want the same engine you use
locally. The big win is for jobs that need Kubernetes.

## Trade-offs, honestly

- **Linux runners only** (`ubuntu-*`, x86_64 and arm64). On macOS the dev
  cluster is a VM, and GitHub's macOS runners allow no nested
  virtualization.
- **No image cache between runs.** The unpacked layers belong to the
  cluster's subordinate user ids, which the runner user cannot read back
  into a cache archive. Pulls from Docker Hub took 0.7 to 6 s per image on
  these runners.
- **The network stack is slirp4netns, not pasta.** On GitHub's runners
  pasta dropped TCP flows now and then (3 of 5 runs hung with Ubuntu's
  pasta, none with slirp4netns), so the action defaults to slirp4netns.
- **Not compared:** the runner's preinstalled kind without the action,
  k3d, and a kind node image cached between runs. Those might close part
  of the gap; we have not measured them.
- **It is not upstream Kubernetes.** It passes 413 of the 419 Kubernetes
  v1.33 conformance specs (k3s: 416), and litek8s is a young 0.x project
  whose flags and formats may still change. If your e2e tests depend on an
  exact kubelet behavior, read [Limitations](/docs/limitations/) first.
  If they are about your chart, your operator or your manifests, that is
  what this is for.

## Try it

Swap the setup step in one workflow and compare the job time in your own
Actions tab:

```yaml
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
```

Pin it to a release tag as above. Every input (`nodes`, `docker`,
`builder`, `version`, `wait`), three workflows to copy (Helm, compose,
multi-node) and the full method are in [CI](/docs/ci/).
