# Contributing to litek8s

Thanks for trying litek8s. Bug reports, compatibility gaps ("this chart /
kubectl command works on k3s but not here") and small fixes are the most
useful contributions right now. For anything larger, open an issue first so
we can agree on the approach.

By contributing you agree that your contribution is licensed under the
Apache License 2.0 (the project's license, see [LICENSE](LICENSE)), as
section 5 of the license describes.

## Using litek8s vs developing it

Keep the two apart on your machine. **Use** litek8s from a release
(`brew install jairoFernandez/tap/litek8s`, install.sh or `cargo install
litek8s`): its clusters, `~/.litek8s/docker.sock` and `litek8s-<cluster>`
contexts are your own. **Develop** it with `dev/lk`, which runs this
checkout's `target/release/litek8s` (rebuilt when a source is newer) with
`LITEK8S_HOME=~/.litek8s-dev`: its own dev VM, clusters, Docker socket,
config, kubeconfig and `lkdev-<cluster>` contexts. A build from a checkout
says so: `litek8s --version` and the UI's header show `DEV <commit>` and
the `LITEK8S_HOME` in use.

```bash
dev/lk build-vm                      # the macOS dev VM's guest from this checkout
dev/lk dev up && dev/lk ui           # a cluster of the checkout's build, next to the release's
eval "$(dev/lk env)"                 # its DOCKER_HOST and KUBECONFIG in this shell
```

Details, the first guest on a new Mac and the pinned `dev/stable.sh` in
[docs/develop-without-docker.md](docs/develop-without-docker.md#using-litek8s-vs-developing-it).

## Build

Rust stable (≥ 1.90; the crate uses edition 2024):

```bash
cargo build                          # debug
cargo build --release                # what the releases ship
```

On macOS, `litek8s dev up` also needs the dev VM's guest (a static Linux
build of litek8s plus an Alpine kernel and userland):

```bash
dev/lk build-vm                      # needs a Docker API for the musl build
```

You do not need Docker or OrbStack for that: a pinned litek8s can provide
the Docker API ([docs/develop-without-docker.md](docs/develop-without-docker.md)).

## Test

```bash
cargo test                           # unit and API tests, no Docker needed
cargo nextest run                    # the same, in parallel processes, as CI runs them
cargo test discovery_and_default_objects        # one test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

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
  `cargo test` pass; CI runs them on Linux and macOS, plus the corpus's
  L0/L1 tiers and the GitHub Actions examples on Linux
  ([docs/ci.md](docs/ci.md#ci-of-litek8s-itself)).
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
