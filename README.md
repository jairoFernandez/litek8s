<p align="center">
  <a href="https://litek8s.dev"><img src="https://raw.githubusercontent.com/jairoFernandez/litek8s/main/web/public/brand/litek8s-mark.svg" width="88" height="88" alt="litek8s"></a>
</p>

<h1 align="center">litek8s</h1>

<p align="center">
  <b>A whole Kubernetes in one binary.</b><br>
  A local cluster in about a second, your spare machines as one cluster,<br>
  and a control plane that idles at 23 MiB.
</p>

<p align="center">
  <a href="https://github.com/jairoFernandez/litek8s/releases"><img src="https://img.shields.io/github/v/release/jairoFernandez/litek8s?color=8b5cf6&label=release" alt="Latest release"></a>
  <a href="https://crates.io/crates/litek8s"><img src="https://img.shields.io/crates/v/litek8s?color=8b5cf6" alt="crates.io"></a>
  <a href="https://github.com/jairoFernandez/litek8s/actions/workflows/test.yml"><img src="https://github.com/jairoFernandez/litek8s/actions/workflows/test.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/jairoFernandez/litek8s/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-8b5cf6" alt="License: Apache-2.0"></a>
</p>

<p align="center">
  <a href="https://litek8s.dev"><b>Website</b></a> ·
  <a href="https://litek8s.dev/docs/"><b>Docs</b></a> ·
  <a href="https://litek8s.dev/docs/quickstart/">Quickstart</a> ·
  <a href="https://github.com/jairoFernandez/litek8s/releases">Releases</a>
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/jairoFernandez/litek8s/main/docs/readme/demo.svg" width="880" alt="litek8s dev up starts a cluster in 1.28 seconds, kubectl get nodes shows it Ready, litek8s ui opens the terminal UI">
</p>

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up        # a cluster, Ready in ~1 s
kubectl get nodes     # your kubectl, Helm and Argo CD work unchanged
```

## Why litek8s

- **A cluster before you finish blinking.** `litek8s dev up` has a node
  Ready in 0.17 s on Linux and 1.28 s on macOS; kind takes 31.6 s and 34.3 s.
- **Small enough to forget it's running.** The whole control plane idles at
  23 MiB where k3s needs 567 MiB, 33.5 MiB with five nginx pods (k3s: 579);
  with Argo CD, 90 MiB against 900 MiB. On macOS a dev cluster runs in a
  small Linux VM and Activity Monitor shows the whole VM: about 240 MiB
  fresh, more once it has pulled and run images.
- **Real Kubernetes, not a look-alike.** It serves the Kubernetes API, so
  kubectl, Helm and Argo CD just work: 119/119 on our compatibility corpus
  (47 Helm charts included) and 413/419 on the official v1.33 conformance suite.
- **Batteries included.** A Docker-compatible API, a terminal UI, metrics,
  log search and alerts (+1.8 MiB, instead of 1.3 GiB for
  kube-prometheus-stack and Loki) and an MCP server for AI agents, all in
  the same binary.
- **No Docker Desktop, no VM to babysit.** On macOS one small VM it manages
  itself, no sudo; on Linux a rootless sandbox.
- **From laptop to rack.** The same binary turns a Raspberry Pi, an old
  laptop and a cheap cloud VM into one cluster, across NATs, with a highly
  available control plane when you want one.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/jairoFernandez/litek8s/main/docs/ui/kube-light.svg">
    <img src="https://raw.githubusercontent.com/jairoFernandez/litek8s/main/docs/ui/kube-dark.svg" width="820" alt="litek8s ui: the Kubernetes view, a k9s-style list of pods with their status">
  </picture>
  <br><sub><code>litek8s ui</code>: your clusters, pods, containers, images and logs in one keyboard-driven terminal UI.</sub>
</p>

## What you can do

- **Keep the clusters you have.** litek8s only adds contexts to your
  kubeconfig, next to EKS, GKE or Civo; same manifests, same charts, and
  going back is deleting it. → [Next to your clusters](https://litek8s.dev/docs/alongside/)
- **Develop locally.** As many clusters as you like, each with as many
  nodes, NodePorts on `localhost` and your code mounted at the same path.
  → [Dev clusters](https://litek8s.dev/docs/dev/)
- **Drop Docker Desktop.** `docker`, `docker compose`, Dev Containers and
  Testcontainers run on the same cluster through its Docker Engine API.
  → [Docker API](https://litek8s.dev/docs/docker/)
- **Kubernetes in CI: the fastest setup step.** One GitHub Actions step
  gives the job a cluster (and a Docker API) Ready in 2.5 s instead of
  kind's 45 s; Helm and multi-node jobs finish 3–5× sooner with 26–37× less
  memory, and 1.8× sooner than kind in Docker in Docker on GitLab-style
  container jobs. → [CI](https://litek8s.dev/docs/ci/)
- **Turn spare machines into a cluster.** Install a server and its nodes
  over SSH, or declare the whole cluster in one file and run
  `litek8s cluster apply -f cluster.yaml`.
  → [Clusters over SSH](https://litek8s.dev/docs/clusters/)
- **Span homes and clouds.** Pods talk across nodes over a WireGuard mesh
  litek8s sets up itself, relayed when two machines sit behind NATs: no VPN,
  no port forwarding. → [Networking](https://litek8s.dev/docs/networking/)
- **Run one LLM across all your machines.** A model too big for any one of
  them runs on several with llama.cpp's RPC backend, as a DaemonSet behind
  one OpenAI-compatible endpoint. → [One LLM across machines](https://litek8s.dev/docs/llm-cluster/)
- **Stay up when a server dies.** Three API servers share the store through
  Raft, no etcd; any one can fail without losing a write.
  → [HA control plane](https://litek8s.dev/docs/ha/)
- **Let AI agents operate it.** `litek8s mcp` gives Claude Code and other
  MCP clients your clusters, pod diagnosis, logs and metrics, with
  guardrails. → [Agents](https://litek8s.dev/docs/agents/)
- **See what's happening.** Prometheus-compatible metrics (Grafana works),
  log search across every pod and alerts to a webhook, ntfy or Telegram.
  → [Observability](https://litek8s.dev/docs/observability/)

## Install

macOS (Apple silicon) and Linux (x86_64, aarch64):

```bash
brew install jairoFernandez/tap/litek8s
```

or

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

On Linux, `litek8s dev setup` checks what rootless clusters need. A server
for your other machines is one command too: `install.sh | sh -s -- server`.
Other options (cargo, releases, servers and nodes):
[Install](https://litek8s.dev/docs/install/).

## How it compares

| | litek8s | reference |
|---|---|---|
| `dev up` to node Ready, Linux / macOS | **0.17 s / 1.28 s** | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s |
| Whole control plane, idle | **23.2 MiB** | k3s minimal 567 MiB |
| Whole orchestrator, 5 nginx pods | **33.5 MiB** | k3s minimal 579 MiB |
| Dev cluster idle memory, Linux | **47 MiB** | kind 526 MiB, k3d 398 MiB |
| Metrics, logs and alerts | **+1.8 MiB** | kube-prometheus-stack + Loki 1,371 MiB |
| Kubernetes v1.33 conformance | **413/419** | k3s 416/419 |

Measured with the scripts in this repository; setups, raw data and caveats
in [Numbers](https://litek8s.dev/docs/numbers/).

## Limitations

litek8s is an early preview (v0.x), built and tested by one person.

- **Authentication and RBAC are simpler than upstream's** (no OIDC, no
  Node authorizer, no certificate revocation). Servers enforce RBAC
  (`--authorization-mode RBAC-audit` only logs); dev clusters listen on
  `127.0.0.1` only and every client there is admin.
- APIs, flags and on-disk formats may change between 0.x releases.
- Some kubelet behaviors are missing.

The full list: [Limitations](https://litek8s.dev/docs/limitations/).
Feedback and issues are very welcome.

## Learn more

- [litek8s.dev](https://litek8s.dev): the tour
- [Documentation](https://litek8s.dev/docs/): [next to your clusters](https://litek8s.dev/docs/alongside/), [how it works](https://litek8s.dev/docs/#how-it-works),
  [platforms](https://litek8s.dev/docs/platforms/), [multi-node and HA](https://litek8s.dev/docs/multi-node/),
  [CRDs](https://litek8s.dev/docs/crds/)
- [Contributing](https://github.com/jairoFernandez/litek8s/blob/main/CONTRIBUTING.md) · [Security](https://github.com/jairoFernandez/litek8s/blob/main/SECURITY.md)

## History

litek8s began as a small Docker-backed orchestrator with its own daemon and
manifest format, plus an experiment in distributed LLM inference. 0.2.0
removed both; every command they had has a Kubernetes-native replacement,
listed in [CHANGELOG.md](https://github.com/jairoFernandez/litek8s/blob/main/CHANGELOG.md).

## License

Apache-2.0, see [LICENSE](https://github.com/jairoFernandez/litek8s/blob/main/LICENSE) and [NOTICE](https://github.com/jairoFernandez/litek8s/blob/main/NOTICE). Third-party
components: [THIRD-PARTY.md](https://github.com/jairoFernandez/litek8s/blob/main/THIRD-PARTY.md).
