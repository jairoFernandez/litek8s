# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## What is litek8s

A Docker-backed Kubernetes-inspired orchestrator demo in Rust. Single binary with a kubectl-like CLI, a local daemon over a Unix socket, SSH-backed remote nodes, and a reconciliation loop. Intentionally minimal (~9k lines).

## Build & Test Commands

```bash
cargo build                    # build
cargo test                     # run integration tests (requires Docker)
cargo run -- serve             # start daemon
cargo run -- apply -f examples/nginx.yaml  # deploy a workload
```

Integration tests require a Docker API (Docker, OrbStack, or litek8s's own: docs/develop-without-docker.md). Tests use `tokio::test(flavor = "multi_thread")` and serialize via an async mutex to avoid Docker socket conflicts. There are no unit tests — all testing is integration-based.

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
- **Manifest** (`manifest.rs`) — YAML parsing and validation. API version `litek8s.dev/v1alpha1`, single replica, single container. Spec changes detected via SHA256 hash.
- **Protocol** (`protocol.rs`) — `Request`/`Response` enums serialized as JSON over async Unix socket streams.
- **Service** (`service.rs`) — Daemon-owned TCP listeners that forward to deployment containers, surviving CLI exits. Supports SSH tunneling for remote nodes.
- **Control Plane** (`control_plane/mod.rs`) — Stub infrastructure for future Raft-based HA (openraft dependency included but not yet active).

## Key Conventions

- Rust edition 2024. Error handling uses `anyhow::Result` with `.context()` chains.
- All async code uses `tokio` with full features.
- State is persisted atomically as JSON; the `Store` uses `RwLock` for reads and a separate `Mutex` for serializing disk writes.
- Manifest validation enforces: `replicas: 1`, exactly one container, `restartPolicy: Always`, allowed resource limit formats (`500m` cpu, `128Mi` memory).
- Config paths use XDG directories via the `directories` crate. Socket at `$XDG_RUNTIME_DIR/litek8s/daemon.sock`, state at XDG data dir.
