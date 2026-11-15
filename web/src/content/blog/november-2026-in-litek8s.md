---
title: "November 2026 in litek8s, first half: 419/419, your own users and docker push"
description: "What shipped in litek8s from v0.2.18 to v0.2.25: full conformance, OIDC and webhook auth, admission for controllers' pods, docker push and GPUs."
date: 2026-11-14
tags: [changelog, release notes, build in public]
docs: [auth, docker, networking, registries, clusters, install, numbers, limitations]
translation: es/noviembre-2026-en-litek8s
---

Eight releases in fifteen days: v0.2.18 came out on October 30, right after
the [October roundup](/blog/october-2026-in-litek8s/), and v0.2.25 on
November 14. This is what changed, grouped by what it means for you rather
than by version. Every item is in the
[CHANGELOG](https://github.com/jairoFernandez/litek8s/blob/main/CHANGELOG.md).

## The five that matter most

**1. 419/419 on the Kubernetes v1.33 conformance suite (0.2.22).** Every
`[Conformance]` spec of the upstream `e2e.test` passes on Linux with crun,
on a two-node rootless dev cluster; k3s passes 416 on the same suite. The
last three failures were a `medium: Memory` emptyDir that was a directory
on disk instead of a tmpfs, an init container that said
`ContainerCreating` where the kubelet says `PodInitializing`, and a runner
that used a kubectl too new for one spec. The story is in
[419 of 419](/blog/419-of-419/).

**2. Your users from OIDC, static tokens or a webhook (0.2.25).** Servers
used to know only certificates signed by their own CA and service account
tokens. `litek8s apiserver` now takes kube-apiserver's
`--oidc-issuer-url`, `--token-auth-file` and
`--authentication-token-webhook-config-file` flags, with their defaults,
so Dex, Keycloak, Google or Entra ID users come in with the login they
already have, and RBAC binds them and their groups like anyone else.
[Who are you?](/blog/who-are-you/) explains the order a token is tried in
and tests it against a small issuer.

**3. Admission webhooks see the controllers' pods (0.2.25).** A
Deployment's pods went around mutating and validating webhooks, so Istio,
Linkerd or Vault Agent never injected their sidecar into them, and Kyverno
or Gatekeeper policies on pods did not hold for them. Every object the
server writes itself now goes through admission as
`system:kube-controller-manager`, and a ReplicaSet whose pods a webhook
refuses records `FailedCreate` and retries.
[The sidecar that never arrived](/blog/the-sidecar-that-never-arrived/).

**4. `docker push` (0.2.25).** The Docker API answered `501` to a push. It
now pushes to any OCI registry with Docker's progress output, and a pulled
`alpine:3.20` pushed to your own registry keeps the digest Docker Hub
publishes for it, although litek8s keeps layers unpacked.
[Pushing what you no longer have](/blog/pushing-what-you-no-longer-have/)
has how, and what we checked.

**5. GPUs without a device plugin (0.2.23).** On crun nodes the agent
reads the Container Device Interface specs that `nvidia-ctk cdi generate`
or `amd-ctk` write, advertises each kind as an extended resource
(`nvidia.com/gpu: 2`), and gives a container that asks for one its own
devices, mounts and hooks. No operator to install. See
[docs/gpus.md](https://github.com/jairoFernandez/litek8s/blob/main/docs/gpus.md).

## Kubernetes behaves more like Kubernetes

- **Invalid objects are refused (0.2.25)**, with kube-apiserver's messages:
  names by kind, label keys and values, workload selectors, pod specs, Job
  restart policies, Service ports. Updates ratchet: what an object stored
  earlier already gets wrong does not block it, only new problems are
  refused. A Deployment stored by 0.2.24 with the label `team: "web team!"`
  still scales on 0.2.25, while a new bad label is refused:

  ```text
  $ kubectl scale deployment web --replicas=3
  deployment.apps/web scaled
  $ kubectl patch deployment web --type merge -p '{"metadata":{"labels":{"tier":"front end"}}}'
  The Deployment "web" is invalid: metadata.labels: Invalid value: "front end": a valid label must be ...
  $ kubectl create -f job.yaml    # restartPolicy: Always
  The Job "j" is invalid: spec.template.spec.restartPolicy: Unsupported value: "Always": supported values: "OnFailure", "Never"
  ```

- **Rollouts that stop progressing fail (0.2.24)** with
  `ProgressDeadlineExceeded`, so `kubectl rollout status` exits instead of
  waiting and Argo CD shows the app Degraded.
- **A Job's deadline passes while its pod is Pending (0.2.21).** A pod no
  node fits never changes, so the controller never looked at the Job
  again. [The pod that waits](/blog/the-pod-that-waits/).
- **The kubelet's events (0.2.22)**: `Unhealthy`, `Killing`, `BackOff` and
  image pull failures, counted on one Event when they repeat.
- **Pods' `imagePullSecrets` are used (0.2.22)**, or their
  ServiceAccount's. [Private registries](/docs/registries/).
- **Fix: a PV with `Retain` lost its data (0.2.19)** when its object was
  deleted. Only `Delete` removes the data now.

## Networking

- **NetworkPolicies on the Docker runtime (0.2.25).** In October they were
  enforced only on crun nodes; macOS servers and other Docker nodes stored
  them without effect. They now enforce the same rules, loaded into
  Docker's network namespace before a selected pod starts. A Docker host
  that cannot filter bridged traffic keeps isolated pods in
  `ContainerCreating` with the reason, instead of running them
  unprotected. Tested on OrbStack so far.
  [Networking](/docs/networking/#networkpolicies).
- **Relays chosen by round trip (0.2.22).** Nodes ping each other over the
  overlay every 10 s, and a pair with no direct path goes through the relay
  with the lowest a → relay → b. `litek8s_overlay_peer_rtt_seconds` shows
  each round trip.

## Faster

Two posts in a series about where litek8s spends its time, both with the
benchmark that found the problem:

- **Lists up to 13× faster (0.2.18)**: a list copies the stored JSON bytes
  instead of parsing and serializing every object. 1,000 ConfigMaps: 507 →
  6,616 lists/s. [Small is not enough](/blog/small-is-not-enough/).
- **`kubectl get` 2.4× faster (0.2.20)**: building a Table copied every
  row twice. [The copy hidden in json!](/blog/the-copy-hidden-in-json/)

`cargo bench --bench api_load` and `--bench micro` came with them, and
every pull request is now compared with its base on the same CI runner.

## Servers and operations

- **`litek8s volume move <claim> --to <node>` (0.2.19)** moves a
  `local-path` volume to another node: scales its workloads down, copies
  and checks the data over SSH, re-creates the PV and claim, and scales
  them back. [Clusters](/docs/clusters/#moving-a-volume-to-another-node).
- **An agent token (0.2.24)** joins nodes and nothing else: it cannot add
  a server to the control plane, and it is no admin credential. Agents get
  it from `install` and `cluster apply`; the join token is for servers.
- **Signed releases with an SBOM (0.2.23)**: every archive has a Sigstore
  build provenance attestation and a CycloneDX SBOM;
  `LITEK8S_VERIFY=1` makes install.sh check it.
  [Verify a release](/docs/install/#verify-a-release).
- **Debian without sbin in PATH (0.2.24)**: `dev up` failed with
  `failed to run iptables` ([#38](https://github.com/jairoFernandez/litek8s/issues/38)).
  litek8s now adds the system directories its PATH lacks.
- **A rootless dev environment that fails to start says so in a second
  (0.2.24)**, with its log, instead of waiting a minute and leaving
  processes behind.

## Also written

- [Small is not enough](/blog/small-is-not-enough/) and
  [The copy hidden in json!](/blog/the-copy-hidden-in-json/), the speed
  series
- [The pod that waits](/blog/the-pod-that-waits/)
- [419 of 419](/blog/419-of-419/)
- [The sidecar that never arrived](/blog/the-sidecar-that-never-arrived/),
  [Pushing what you no longer have](/blog/pushing-what-you-no-longer-have/)
  and [Who are you?](/blog/who-are-you/), about 0.2.25

## Still missing

Of the gaps the October roundup listed, OIDC, static tokens and webhook
authentication, `imagePullSecrets` and `docker push` are done. What is
known to be missing today ([Limitations](/docs/limitations/)):

- one OIDC issuer only (no `--authentication-config`), the `TokenReview`
  API checks only service account tokens, and client certificates cannot
  be revoked;
- `docker push` sends one platform, never a multi-platform index;
- application `/metrics` endpoints are not scraped;
- no HorizontalPodAutoscaler controller yet.

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
