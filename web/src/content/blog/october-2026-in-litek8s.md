---
title: "October 2026 in litek8s: 21 releases, RBAC, NetworkPolicies and a UI for any cluster"
description: "What shipped in litek8s in October 2026: RBAC and NetworkPolicies enforced, a GitHub Action for CI, a terminal UI for any cluster, and litecd for GitOps."
date: 2026-10-30
tags: [changelog, release notes, build in public]
docs: [auth, networking, ci, ui, registries, litecd, numbers, limitations]
translation: es/octubre-2026-en-litek8s
---

October was litek8s's first month in public. The first release, v0.1.0, was
tagged on October 20; by October 30 there had been 21, ending at v0.2.17.
This is what changed, grouped by what it means for you rather than by
version. Every item is in the
[CHANGELOG](https://github.com/jairoFernandez/litek8s/blob/main/CHANGELOG.md).

## The five that matter most

**1. Servers authenticate clients and enforce RBAC (0.2.12).** Until then,
every client that reached a server's port was cluster admin, token or not:
any pod could read every Secret. `litek8s apiserver` now authenticates
service account tokens, client certificates and the join token, and
enforces Role, ClusterRole and their bindings, with
`--authorization-mode RBAC` as the default. `RBAC-audit` evaluates
everything, allows it anyway and logs each denial, so you can find the RBAC
your charts never shipped before you enforce. Dev clusters are unchanged.
This was breaking for servers: [Authentication and RBAC](/docs/auth/) has
the upgrade steps for a 401 and for workloads that get 403.

**2. NetworkPolicies are enforced (0.2.12).** They used to be stored and
served while every pod reached every pod. Now ingress and egress,
pod and namespace selectors, `ipBlock` with `except`, named and ranged
ports over TCP, UDP and SCTP are enforced on crun nodes (Linux), across
nodes over the WireGuard overlay, and a selected pod's rules are loaded
before its first container starts. The Docker runtime on macOS still
stores them without effect. [Networking](/docs/networking/).

**3. A GitHub Action for CI (0.2.1).** One step gives a job a cluster and a
Docker API, replacing `helm/kind-action` and the runner's dockerd:

```yaml
jobs:
  e2e:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
      - run: |
          helm install hello ./chart --wait
          helm test hello
```

Measured on GitHub's `ubuntu-24.04` runners, medians of 5 runs:

| Helm chart e2e | litek8s | kind |
|---|---|---|
| cluster ready | 2.6 s | 44.7 s |
| whole job | 21 s | 79 s |
| memory peak | 42 MiB | 549 MiB |

Setup, ranges and the compose and multi-node examples: [CI](/docs/ci/).

**4. `litek8s ui` works on clusters that are not litek8s (0.2.13–0.2.17).**
Home now shows k3s, kubeadm and managed clusters properly: the distribution
and version, each node's CPU and memory from `metrics.k8s.io` against its
allocatable, with history. Under the meters it shows what the pods
*reserve* (requests and limits against allocatable, and how many pods have
no limit), with `│` and `┃` marks on the meters where requests and limits
reach. A new Apps view (`0`) lists Argo CD and litecd Applications with
sync, health, history and per-resource results, and can sync, refresh and
open diffs through the Kubernetes API alone. [Terminal UI](/docs/ui/).

**5. litek8s got smaller on purpose (0.2.0).** The original Docker-backed
daemon and the experimental distributed LLM compute were removed: about
25,000 lines of Rust, each with a Kubernetes-native replacement that had
already shipped. The release binary went from 38.7 MiB to 32.2 MiB (macOS
arm64, stripped). The CHANGELOG has a table from every removed command to
what to use instead.

## Running on real servers

- **LoadBalancer Services listen on the machine's own ports** (80, 443) on
  servers and nodes, as k3s's servicelb does, so an ingress controller is
  reachable on a cloud VM's public address (0.2.5). The API server leaves
  port 443 to them (0.2.6–0.2.7), and LoadBalancer status prefers a node's
  ExternalIP so external-dns publishes the public address (0.2.12).
- **Private registries and mirrors** in k3s's `registries.yaml` format: a
  k3s file works as is, for pod pulls and the Docker API (0.2.3).
  [Private registries and mirrors](/docs/registries/).
- **Cluster DNS** resolves outside names on hosts whose first nameserver is
  IPv6 (0.2.6), and nodes that join while the server is starting get the
  cluster DNS (0.2.3).

## Kubernetes compatibility

Most of these came from running real software and watching what broke:

- `kubectl exec` and port-forward over WebSocket no longer drop after a
  minute: the server answers client-go's pings (0.2.12).
- JSON patch `replace` of a missing object member sets it, as upstream's
  json-patch does; Vault's HA labels depend on it (0.2.12).
- OpenAPI v2 and v3 for the built-in types, so Argo CD's server-side apply
  and diff work and `kubectl explain` shows built-in types (0.2.2).
- CRD printer columns with filters show their values: cert-manager's
  Certificates no longer look unready in `kubectl get` (0.2.11).
- exec and attach over SPDY for client-go's `NewSPDYExecutor`; `hostPath`
  `type`; `mountPropagation`; pods may bind ports below 1024 and ping
  without privileges; `fsGroup` can read `defaultMode: 0600` files (0.2.17).
- Pod certificates and ClusterTrustBundles, enough for Agent Substrate to
  install and run on a single crun node (0.2.17,
  [substrate.md](https://github.com/jairoFernandez/litek8s/blob/main/docs/substrate.md)).

## Dev clusters

- The dev VM's disk has a size you can set (`disk:` in config.yaml,
  `dev up --disk`), grown in place with data kept (0.2.9).
- `sudo litek8s dev setup` installs what a rootless dev cluster lacks
  (crun, uidmap, passt, iproute2, iptables) with the system's package
  manager (0.2.11).
- `docker run -d` and `docker compose up -d` return at once again when
  litek8s inherited `NOTIFY_SOCKET` from systemd or a tool (0.2.10).

## Numbers we published

- The whole control plane idles at **23.2 MiB** on the v0.2.1 release
  binary, against 567 MiB for k3s minimal, on a 1 GiB x86_64 VM.
- Built-in metrics, logs and alerts add **1.8 MiB**, against 1,371 MiB for
  kube-prometheus-stack plus Loki on the same workload; what that
  comparison leaves out is in
  [its own post](/blog/observability-without-kube-prometheus-stack-and-loki/).

All of them, with their sources: [Numbers](/docs/numbers/).

## Next to litek8s: litecd

[litecd](https://github.com/ideascoldigital/litecd), a separate project,
is a GitOps controller that reads Argo CD's `Application`, `AppProject` and
`ApplicationSet` manifests unchanged: 20 apps idle in 6.9 MiB against Argo
CD's 310 MiB on the same litek8s cluster. It now has a docs page,
[GitOps with litecd](/docs/litecd/), and two posts from putting it on a
production cluster:
[Replacing Argo CD's controller with litecd](/blog/replacing-argo-cd-with-litecd/)
and [The node that was too full](/blog/the-node-that-was-too-full/).

## Also written this month

- [You don't need a space shuttle to ship your app](/blog/you-dont-need-a-space-shuttle/)
- [Kubernetes on a Raspberry Pi and an old laptop](/blog/raspberry-pi-kubernetes-cluster/)
- [Cloning a production cluster onto litek8s](/blog/cloning-a-production-cluster/)
- For agents: litek8s.dev serves `/llms.txt` and `/llms-full.txt`, built
  from the docs (0.2.12).

## Still missing

From [Limitations](/docs/limitations/) and the docs, the gaps that are
known today:

- no OIDC, static token or webhook authenticators, and no client
  certificate revocation;
- application `/metrics` endpoints are not scraped;
- no HorizontalPodAutoscaler controller yet;
- pods' `imagePullSecrets` are not used, and the Docker API cannot
  `docker push` yet.

## Try it

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up
litek8s ui
```

Or, without Homebrew:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Issues and ideas go to
[github.com/jairoFernandez/litek8s](https://github.com/jairoFernandez/litek8s).
