# Contributing to litek8s

Thanks for trying litek8s. Bug reports, compatibility gaps ("this chart /
kubectl command works on k3s but not here") and small fixes are the most
useful contributions right now. For anything larger, open an issue first so
we can agree on the approach.

By contributing you agree that your contribution is licensed under the
Apache License 2.0 (the project's license, see [LICENSE](LICENSE)), as
section 5 of the license describes.

## Build

Rust stable (≥ 1.90; the crate uses edition 2024):

```bash
cargo build                          # debug, with the experimental `llm` feature
cargo build --release --no-default-features   # what the releases ship
```

On macOS, `litek8s dev up` also needs the dev VM's guest (a static Linux
build of litek8s plus an Alpine kernel and userland):

```bash
dev/vm/build.sh                      # needs a Docker API for the musl build
```

You do not need Docker or OrbStack for that: a pinned litek8s can provide
the Docker API ([docs/develop-without-docker.md](docs/develop-without-docker.md)).

## Test

```bash
cargo test                           # unit tests + integration tests
cargo test --no-default-features     # CI runs both
cargo test apply_get_logs_and_delete_roundtrip   # one test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

- `tests/integration.rs` drives the legacy daemon and needs a Docker API
  (Docker, OrbStack, or litek8s's own: `eval "$(dev/stable.sh env)"`); it
  skips itself when none is reachable.
- `tests/kube_apiserver.rs`, `tests/mcp.rs`, `tests/obs_api.rs` and
  `tests/ha.rs` run the API server in-process and need no Docker.
- Unit tests live next to the code (`#[cfg(test)]`). The terminal UI is
  tested with ratatui's TestBackend at 80x24 (`src/ui/tests.rs`).
- crun runtime and rootless sandbox code only runs on Linux; `bench/vm.sh`
  starts an Ubuntu VM to try it from a Mac.

## Compatibility corpus

The yardstick for "does it behave like Kubernetes" is the corpus in
[compat/](compat/README.md): kubectl and Helm workflows in tiers L0 to L4.
A change to the API server, controllers or node agent should keep it at
100 %:

```bash
cargo run --release -- apiserver --kubeconfig /tmp/litek8s.kubeconfig &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh --tiers L0,L1   # quick, API only
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh                 # everything (needs helm, ~15 min)
compat/baseline-k3d.sh                                           # the same cases on k3s
```

Needs kubectl, perl and python3 (and helm for L4). A new case must pass on
k3s (k3d) before it counts. The official conformance suite runs with
`compat/conformance.sh` (~30 min, see [compat/CONFORMANCE.md](compat/CONFORMANCE.md)).

## Pull requests

- Keep each PR to one change, with tests where the code has them.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test` (both feature sets) pass; CI runs them on Linux and macOS.
- Performance claims come with numbers and how they were measured; the
  harnesses are in [bench/](bench/).
- Comments and docs say what the code does and why, in plain sentences.

## Releases

A `v*` tag matching `Cargo.toml`'s version runs
[.github/workflows/release.yml](.github/workflows/release.yml): static
Linux binaries (x86_64, aarch64), the macOS arm64 binary (ad-hoc signed with
the Virtualization entitlement) with the dev VM's guest, and `SHA256SUMS`,
attached to a draft release. Running the workflow by hand builds the same
archives as artifacts without releasing.
