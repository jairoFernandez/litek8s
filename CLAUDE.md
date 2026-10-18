# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is litek8s

A Docker-backed Kubernetes-inspired orchestrator in Rust, aimed at cheap and heterogeneous home/edge hardware. Single binary with a kubectl-like CLI, a local daemon over a Unix socket, SSH-backed remote nodes, and a reconciliation loop. It is moving toward the Kubernetes object model as its core: `litek8s apiserver` serves the real Kubernetes API (`src/kube/`), measured by the compatibility corpus in `compat/`.

## Build & Test Commands

```bash
cargo build                    # build
cargo test                     # run integration tests (requires Docker)
cargo run -- serve             # start daemon
cargo run -- apply -f examples/nginx.yaml  # deploy a workload
```

Integration tests in `tests/integration.rs` require a running Docker daemon; they use `tokio::test(flavor = "multi_thread")` and serialize via an async mutex to avoid Docker socket conflicts. Unit tests live next to the code (`#[cfg(test)]`). `tests/kube_apiserver.rs` exercises the API server over HTTP and needs no Docker.

Kubernetes compatibility corpus (needs kubectl, perl, python3; L4 needs helm):
```bash
cargo run --release -- apiserver --kubeconfig /tmp/litek8s.kubeconfig &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh --tiers L0,L1
compat/baseline-k3d.sh          # reference score on real k3s; every case must pass there
```

Official conformance suite (e2e.test v1.33, ~30 min; scores in `compat/CONFORMANCE.md`):
```bash
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s
```

To run a single test:
```bash
cargo test apply_get_logs_and_delete_roundtrip
```

## Architecture

**CLI → Unix Socket → Daemon → Reconciler → Docker Runtime**

- **CLI** (`cli.rs`) — Parses commands via clap, sends JSON requests over a Unix socket to the daemon.
- **Daemon** (`daemon.rs`) — Async server holding all runtime state (`AppState`). Dispatches requests, spawns the reconciliation loop (every 2s), cluster snapshot updater, and Docker event listener.
- **Store** (`store.rs`) — In-memory state behind `RwLock`, persisted as JSON to `~/.local/share/litek8s/state.json`. Tracks deployments, services, nodes, and events (capped at 250).
- **Reconciler** (`reconciler.rs`) — Control loop: for each deployment, selects a node, ensures the correct container exists (creating/recreating as needed), probes readiness, and applies exponential restart backoff (max 15s).
- **Docker Runtime** (`runtime/docker.rs`) — Wraps `bollard` for container lifecycle. Containers are labeled with `litek8s.managed=true`, `litek8s.deployment=<name>`, and `litek8s.spec-hash=<hash>` for tracking.
- **Node** (`node.rs`) — Abstracts local vs remote nodes. Remote nodes use an SSH agent protocol (JSON-RPC over stdin/stdout of `ssh <host> litek8s agent`).
- **Manifest** (`manifest.rs`) — YAML parsing and validation. API version `litek8s.dev/v1alpha1`, `replicas >= 1`, single container. Per-container identity is `pod_template_hash()` (template only), so scaling never recreates running replicas.
- **Protocol** (`protocol.rs`) — `Request`/`Response` enums serialized as JSON over async Unix socket streams.
- **Service** (`service.rs`) — Daemon-owned TCP listeners that forward to deployment containers, surviving CLI exits. Supports SSH tunneling for remote nodes.
- **Control Plane** (`control_plane/`) — openraft. Every state change is proposed through `ControlPlaneNode::propose`; a daemon without `--join` initializes a single-node cluster.
- **Kubernetes API server** (`kube/`) — `litek8s apiserver`, separate from the daemon for now. Generic JSON object store with an append-only WAL (`store.rs`), resource registry incl. CRDs (`registry.rs`), discovery incl. aggregated v2 and a schema-less `/openapi/v3` (`discovery.rs`), merge/JSON/strategic patch and simplified server-side apply (`patch.rs`), selectors, server-side tables, watch with resourceVersion resume (`server.rs`), admission webhooks (`admission.rs`), API aggregation for APIServices (`aggregator.rs`), TLS on the same port for in-cluster clients (`tls.rs`). `protobuf.rs` decodes kubectl's protobuf request bodies using the vendored `proto/` schemas, compiled by `build.rs` with protox.
- **Controllers** (`kube/controllers/`) — in-process, driven by the store's change feed: Deployment, ReplicaSet, StatefulSet, DaemonSet, Job, CronJob, ownerReference GC, the scheduler (binds pods to Nodes, provisions local PVs), Service status/Endpoints. No-op writes are skipped so the loop settles.
- **Node agent** (`kube/agent/`) — runs the pods bound to its node. One persistent framed connection to the API server (`protocol.rs`; in-process pipe locally, HTTP upgrade for `litek8s node-agent --server ...`) carries pod events, status and multiplexed log/exec/port-forward streams (`hub.rs` is the server side). Pods run behind the `PodRuntime` trait (`runtime.rs`), picked with `--runtime auto|crun|docker` (auto = crun on a Linux host with crun installed, running as root, else Docker):
  - **crun** (`crun/`, Linux, no daemon): images pulled for the host platform into a content-addressed store, one unpacked dir per layer (`image.rs`); overlayfs rootfs per container; OCI `config.json` with Docker-equivalent defaults (`spec.rs`); pod network/IPC namespaces kept as bind mounts in /run/netns (no pause container) on the `litek8s0` bridge with on-disk IPAM (`net.rs`); one `litek8s crun-shim` per container (same binary, dispatched in `main.rs` before tokio) that runs crun, writes CRI-format logs and the exit status, so containers survive agent restarts (`shim.rs`). Unit tests cover config.json, overlay args, IPAM, logs and unpacking; container execution is only tested on Linux (see bench/vm.sh).
  - **Docker** (macOS dev): a busybox sandbox owns the network namespace, containers join it.

  `kubelet.rs` serves `/metrics/resource` on :10250.

## Key Conventions

- Rust edition 2024. Error handling uses `anyhow::Result` with `.context()` chains.
- All async code uses `tokio` with full features.
- State is persisted atomically as JSON; the `Store` uses `RwLock` for reads and a separate `Mutex` for serializing disk writes.
- Manifest validation enforces: exactly one container, `restartPolicy: Always`, allowed resource limit formats (`500m` cpu, `128Mi` memory).
- Config paths use XDG directories via the `directories` crate. Socket at `$XDG_RUNTIME_DIR/litek8s/daemon.sock`, state at XDG data dir.
