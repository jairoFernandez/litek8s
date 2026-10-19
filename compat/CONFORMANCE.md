# Kubernetes conformance

The [compatibility corpus](README.md) measures what real kubectl and Helm
workflows need. This file tracks the other yardstick: the official
Kubernetes conformance suite, the `[Conformance]` specs of the upstream
`e2e.test` binary that certified distributions must pass.

## Score

Kubernetes **v1.33.13** conformance suite, 419 specs, on 2026-10-18:

| | passed | wall clock |
|---|---|---|
| k3s v1.33.4 (k3d, reference) | 416/419 | 39 min |
| **litek8s, macOS (Docker runtime)**, last complete run (r7) | **413/419** | 29 min |
| litek8s, macOS, final code, parallel pass only (r8) | 385/390 | 20 min |
| litek8s, Linux (crun runtime), the specs macOS cannot run | 9/9 | 1 min |

r7 failed six specs: the five below, which macOS cannot run, and
`Watchers should observe add, update, and delete watch notifications`, a
watch that ended when a burst of writes outran the change feed. Watches now
catch up from the store's history instead (r8 passed it). r8, on the final
code, failed only the five macOS specs in its parallel pass; its serial
pass (29 specs) was cut short when the host disk filled up and Docker
stopped, so it has no complete score. The 29 serial specs passed in r6 and
r7. The five macOS failures all pass on Linux with crun (a focused run of
the DNS and HostPort specs, not the whole suite).

Per SIG, litek8s (macOS) against k3s:

| SIG | k3s | litek8s r7 |
|---|---|---|
| api-machinery | 93/94 | 93/94 |
| apps | 59/60 | 60/60 |
| architecture | 1/1 | 1/1 |
| auth | 10/10 | 10/10 |
| cli | 17/17 | 17/17 |
| instrumentation | 4/4 | 4/4 |
| network | 43/43 | 38/43 |
| node | 89/89 | 89/89 |
| scheduling | 10/11 | 11/11 |
| storage | 90/90 | 90/90 |

The three k3s failures (a `Namespaces` patch and a DaemonSet spec timed
out, a preemption spec lost its connection) happened while litek8s runs
shared the same Docker VM; they look like load, not gaps.

### What fails on macOS, and why

- **DNS (4 specs).** Each pod queries every name over UDP *and* TCP. The
  cluster DNS server answers TCP on Linux, but on macOS port 53 belongs to
  mDNSResponder (binding the wildcard address fails with `EADDRINUSE`) and
  binding a specific address needs root. UDP works, so the TCP half fails.
  All DNS specs pass on Linux with crun.
- **HostPort (1 spec).** Three pods share a hostPort on different hostIPs
  and protocols. Docker Desktop and OrbStack run containers in a VM whose
  addresses differ from the node's, so a hostIP binding to the node
  address cannot be made. Passes on Linux with crun.

## Progression

| run | passed | wall clock |
|---|---|---|
| r1 | 0/419 | (stopped) |
| r2 | 271/419 | 77 min |
| r3 | 346/419 | 59 min |
| r4 | 388/419 | 40 min |
| r5 | 405/419 | 47 min |
| r6 | 412/419 | 27 min |
| r7 | 413/419 | 29 min |
| r8 | 385/390 parallel (serial cut short) | 20 min |

r1 stopped at `BeforeSuite`: the node list with
`fieldSelector=spec.unschedulable=false` came back empty (an absent field
must match `false`). What it took from there, by area:

- **Wire formats.** kubectl and client-go send protobuf: embedded structs
  are flattened, a few proto field names differ from the JSON ones, and
  zero scalars (always on the wire) are dropped unless the Go field is a
  pointer (`src/kube/proto_pointers.rs`, generated from the Go types).
  Strict field validation, duplicate-field errors, server-side tables for
  watches.
- **API machinery.** A CEL interpreter for ValidatingAdmissionPolicy and
  webhook `matchConditions`; CRD schemas (pruning, defaults, validation,
  OpenAPI v2/v3), conversion webhooks, field selectors on custom
  resources; list continue tokens with expiry; watch resume, selector
  transitions and catch-up after a burst of writes; graceful and
  foreground/orphan deletion; ResourceQuota (extended resources too) and
  LimitRange admission; service-account tokens (TokenRequest, TokenReview,
  OIDC discovery); API aggregation (APIService availability, discovery and
  proxying, `src/kube/aggregator.rs`).
- **Controllers.** ReplicationControllers, ControllerRevisions for
  StatefulSets and DaemonSets, Job pod-failure and success policies,
  PodDisruptionBudgets and eviction, scheduler preemption, taints and
  extended resources, EndpointSlices recreated when deleted.
- **Node agent.** Lifecycle hooks, termination messages, ephemeral
  containers, sysctls, kubelet-managed `/etc/hosts` and `resolv.conf`,
  service environment variables, container start order, pods kept
  `Pending` while their images are first pulled, hostPort conflicts and
  `hostIP` on crun, session affinity in the service proxy, pod and
  service `proxy` subresources, SRV/PTR/ExternalName in the cluster DNS.

Raw results (JUnit XML of the `[Conformance]` specs, the gzipped ginkgo
log, `summary.md`) are in `results/conformance-*`.

Compatibility corpus on this branch, merged with main (79bef74): 101/101
(`results/20261018-174352-litek8s-conformance-merged/`).

## Setup

- **Runner.** `compat/conformance.sh` downloads `kubernetes-test` for the
  host (`e2e.test` and `ginkgo`, v1.33.13) into `compat/.work/` and runs it
  from the host like the upstream conformance image does: the parallel
  specs on 8 ginkgo workers, then the `[Serial]` ones on one, with
  `--provider=skeleton`. `compat/conformance_summary.py` writes the
  summary and trims the JUnit files to the `[Conformance]` cases.
- **litek8s.** `litek8s apiserver` (release build, in-memory store) with
  its controllers, scheduler, DNS and service proxy, plus a second node:
  `litek8s node-agent --server http://127.0.0.1:6443 --node-name <host>-2
  --runtime docker`. Several specs (pod anti-affinity, DaemonSets,
  preemption) need two schedulable nodes. Both agents share the host's
  Docker (OrbStack on an Apple M3 Max) and keep their containers apart by a
  node label.
- **Reference.** The k3d cluster of `compat/baseline-k3d.sh` (one server,
  one agent), same runner.
- **Linux.** The same build in a privileged Ubuntu 24.04 container on an
  x86_64 Linux workstation, `--runtime crun`, a single node, focus
  `DNS .*\[Conformance\]|HostPort .*\[Conformance\]`.

```bash
cargo build --release
target/release/litek8s apiserver --kubeconfig /tmp/litek8s.kubeconfig &
target/release/litek8s node-agent --server http://127.0.0.1:6443 \
  --node-name "$(hostname -s | tr A-Z a-z)-2" --runtime docker &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s \
  --focus 'Aggregator.*\[Conformance\]' --no-serial   # one spec
```

## Notes

- The suite is hard on the node: the ValidatingAdmissionPolicy spec
  creates Deployments in a loop until a CEL policy denies them. Before CEL
  was implemented that churned through ~6000 pods and wedged Docker. The
  node agent now also sweeps containers of pods it no longer knows every
  minute.
- litek8s has no authorization: every SubjectAccessReview is allowed.
  Aggregated requests reach extension API servers without a front-proxy
  identity, so they see them as anonymous and their delegated checks pass.
- Not covered by conformance and not tested here: etcd-backed durability,
  HA control planes, cloud providers, Windows nodes.
