# Developing litek8s without Docker

Everything the project used Docker (OrbStack, Docker Desktop) for on a Mac
now runs on litek8s itself: `litek8s dev` (a Virtualization.framework VM
with crun, [dev.md](dev.md)) and the Docker Engine API it serves on
`~/.litek8s/docker.sock` ([docker.md](docker.md)).

| what | needs | on litek8s |
|---|---|---|
| the compat corpus | a cluster | the `litek8s dev` cluster itself (`KUBECONFIG`) |
| the dev VM's guest binary (`dev/vm/build-guest-binary.sh`), Linux bench binaries (`bench/build-linux.sh`) | `docker run rust:*` with the checkout bind-mounted | same scripts, `DOCKER_HOST` on litek8s |
| the k3s reference (`compat/baseline-k3d.sh`) | k3d: privileged containers with nested containerd | k3d on litek8s's Docker API, or k3s in a qemu VM (`compat/baseline-k3s-vm.sh`) |

OrbStack (or Docker Desktop) can stay installed as a fallback: nothing here
stops, reconfigures or uses it. Unset `DOCKER_HOST` and it is what
`docker` talks to again.

## Using litek8s vs developing it

Up to three litek8s can live on one Mac without sharing anything:

| | what | binary | `LITEK8S_HOME` | contexts |
|---|---|---|---|---|
| **using** it | the release | `brew install jairoFernandez/tap/litek8s` (or install.sh) | unset: `~/.litek8s`, the data directory, `~/.kube/config` | `litek8s-<cluster>` |
| **developing** it | this checkout | `dev/lk` (builds `target/release/litek8s` when stale) | `~/.litek8s-dev` | `lkdev-<cluster>` |
| **testing with** it | a pinned revision | `dev/stable.sh` (= `dev/lk stable`) | `~/.litek8s/stable` | `lkstable-<cluster>` |

`LITEK8S_HOME=<dir>` moves every per-user path under `<dir>`: the Docker
sockets (`docker.sock`, `<cluster>/docker.sock`), `config.yaml`, `ui.yaml`
and the UI's state, the kubeconfig `dev up` writes (`<dir>/kubeconfig`
unless `KUBECONFIG` is set), the data directory (`<dir>/data`: the dev VM,
its disk, the clusters, `vm/` for the guest). Contexts get the prefix `lkdev-`
(`LITEK8S_CONTEXT_PREFIX`), the default cluster's docker context is `lkdev`.
So `dev/lk dev up` starts a VM of its own, next to the release's, and
`litek8s ui` / `dev/lk ui` each show only their own clusters. Both
binaries say which one they are: a build from a checkout prints
`litek8s 0.1.0 DEV <commit> (LITEK8S_HOME=...)` and the UI's header shows
`DEV <commit> · ~/.litek8s-dev`; release builds (`LITEK8S_RELEASE=1`, set
by the release workflow) print the plain version.

```bash
dev/lk --version                    # builds target/release/litek8s first if a source is newer
dev/lk build-vm                     # the VM's guest from this checkout, into ~/.litek8s-dev/data/vm
dev/lk dev up --cpus 4 --memory 4096
dev/lk ui
eval "$(dev/lk env)"                # DOCKER_HOST, KUBECONFIG of ~/.litek8s-dev for this shell
dev/lk dev delete                   # and rm -rf ~/.litek8s-dev to forget it entirely
```

`dev/lk build-vm` compiles the guest on `~/.litek8s-dev`'s own Docker API
when its VM runs, else on `DOCKER_HOST` / the docker context. The very
first guest needs a VM to build in: boot the release's guest once with
`LITEK8S_DEV_VM_ASSETS="$(brew --prefix)/share/litek8s/vm" dev/lk dev up`,
then `dev/lk build-vm && dev/lk dev down && dev/lk dev up`.
`dev/lk env` does not export `LITEK8S_HOME`, so `litek8s` from the PATH
in that shell is still the release.

## The rule: develop with a pinned litek8s

The cluster and Docker API you develop **with** must not be the code you
are **editing**: a broken build would take down the tools that test it.
`dev/stable.sh` keeps a known-good litek8s in `~/.litek8s/stable`
(`$LITEK8S_STABLE`): the macOS binary, the VM's kernel, root image and
initrd (with the static Linux litek8s as `/init`) and the revision. The
directory is also its `LITEK8S_HOME`: its own VM and cluster (`data/`),
`docker.sock` and kubeconfig (`~/.kube/config` is not touched), apart from
the release and from `dev/lk`. For a binary pinned before `LITEK8S_HOME`
existed, the script sets the single-path variables (`LITEK8S_DEV_HOME`,
`LITEK8S_DOCKER_SOCKET`, `LITEK8S_CONFIG`, `KUBECONFIG`) to the same places.

```bash
dev/stable.sh install origin/main   # or any rev; builds both binaries, see below
dev/stable.sh up --cpus 4 --memory 4096
eval "$(dev/stable.sh env)"         # DOCKER_HOST and KUBECONFIG for this shell
dev/stable.sh status | down | reset
```

`install` builds the macOS binary with your cargo (target dir
`target/stable-host`) and the VM's Linux binary in a `rust:1.90-alpine`
container on whatever Docker API is current: the running stable cluster's,
when there is one, so after the first install litek8s builds its own next
version. Its cargo cache lives in the Docker volume `litek8s-guest-build`
(inside the VM's disk, not in the checkout). Upgrading:
`dev/stable.sh install <rev> && dev/stable.sh down && dev/stable.sh up`.

**First install on a new Mac.** The guest is a Linux binary, so the very
first one needs some Linux: OrbStack/Docker Desktop if installed (leave
`DOCKER_HOST` unset), a bench VM (`BENCH_BUILD_VM`, see
[bench/build-linux.sh](../bench/build-linux.sh)), or another machine's
`target/guest-musl-arm64/release/litek8s` passed to `dev/vm/build-guest.py`.
There are no published releases yet; a release workflow that uploads the
macOS binary and the VM's `Image`/`root.img`/`initrd.gz` would remove this step.

Keep the footprint modest on a shared laptop: `--cpus 4 --memory 4096`
(the defaults are half the Mac's CPUs and 4096 MiB), `CARGO_BUILD_JOBS=4`
for builds in the VM, one build at a time.

## cargo test

`cargo test` needs no Docker API: the tests that drove the legacy daemon
through Docker (`tests/integration.rs`) went with it in 0.2.0. The API
server, HA, MCP and observability tests run in-process
([CONTRIBUTING.md](../CONTRIBUTING.md#test)); the crun runtime's container
execution is tested on Linux only (bench/vm.sh).

## The compat corpus

The `litek8s dev` cluster is a cluster like any other:

```bash
eval "$(dev/stable.sh env)"
export HELM_CACHE_HOME=$PWD/compat/.work/helm/cache HELM_CONFIG_HOME=$PWD/compat/.work/helm/config HELM_DATA_HOME=$PWD/compat/.work/helm/data
compat/run.sh --target litek8s-dev-macos-vm
```

Result on 2026-10-19: **119/119** (all tiers), in
[compat/results/20261019-110349-litek8s-dev-macos-vm](../compat/results/20261019-110349-litek8s-dev-macos-vm/).
It took 37 min at 4 vCPUs while the Mac was busy with other work (load
average 60–100); the 101-case corpus took 12.3 min on an idle Mac
(compat/RESULTS.md). A cluster you
also use by hand can make cases fail on leftovers (one Helm release
already installed by hand did); `litek8s dev reset` gives a clean one.

This measures the **stable** litek8s. To measure the code you are
editing, boot a guest built from your checkout (next section) and run the
same command.

## Linux builds

```bash
eval "$(dev/stable.sh env)"
CARGO_BUILD_JOBS=4 dev/vm/build-guest-binary.sh          # static musl arm64, the VM's /init
CARGO_BUILD_JOBS=4 bench/build-linux.sh arm64            # glibc, what `node add --install` runs
```

Both bind-mount the checkout (virtiofs into the VM). With
`LITEK8S_BUILD_VOLUME=<volume>` cargo's target dir and registry stay in a
Docker volume on the VM's disk instead, and only the binary is copied out.

Cold build = empty target dir and cargo registry, `rust:*` image already
pulled; incremental = `touch src/lib.rs` after it. Same machine and
conditions as above, `CARGO_BUILD_JOBS=4`:

| build | litek8s, bind mount | litek8s, `LITEK8S_BUILD_VOLUME` | OrbStack, bind mount |
|---|---|---|---|
| `dev/vm/build-guest-binary.sh` (musl), cold | 293 s | 254 s | 300 s |
| same, incremental | 193 s | 199 s | 238 s |
| `bench/build-linux.sh arm64` (glibc), cold | 130 s | 124 s | 215 s |

virtiofs is not the bottleneck: the volume saves 13 % on a cold musl build
and nothing on an incremental one, so the scripts keep the bind mount by
default. (The musl build is slower than the glibc one mostly because rustc
itself runs on musl's allocator.) `dev/stable.sh install` (macOS release
build + guest in a volume + kernel/initramfs) took 473 s cold.

Under heavy host load the same musl cold build took 700 s, and an
incremental one 600 s: the VM's 4 vCPUs compete with the host like any
other process.

### Working on the guest or the Docker API itself

The VM's `/init` is litek8s, so changes under `src/dev/`, `src/docker/` or
`src/kube/` reach the VM only through a new guest. `dev/lk build-vm`
builds one from the checkout (on `~/.litek8s-dev`'s own Docker API when its
VM runs, else the stable cluster's after `eval "$(dev/stable.sh env)"`) and
the next `dev/lk dev up` boots it, while the stable cluster keeps running:

```bash
dev/lk build-vm
dev/lk dev down && dev/lk dev up --cpus 4 --memory 4096
# ... test against ~/.litek8s-dev; the stable cluster is untouched
```

The guest build is the slow part of this loop: the `litek8s` crate alone
at `opt-level=3` takes most of it, even incrementally.

## The k3s reference

Every corpus case must pass on real k3s. `compat/baseline-k3d.sh` (k3d,
1 server + 1 agent) runs unchanged on litek8s's Docker API:

```bash
eval "$(dev/stable.sh env)"
compat/baseline-k3d.sh                    # k3d cluster create ... then compat/run.sh
k3d cluster delete litek8s-compat
```

The k3d nodes are privileged containers running their own containerd
inside the dev VM. Result on 2026-10-19: **119/119**
([compat/results/20261019-114221-k3d-on-litek8s](../compat/results/20261019-114221-k3d-on-litek8s/)),
`k3d cluster create` in 11 s, L0–L3 in 4.4 min, L4 in 36 min, both k3s
nodes together ~1.3 GiB inside the 4 GiB VM. Two gaps in the Docker API
were in the way and are fixed: Docker matches `name` and `id` filters as
regular expressions (k3d looks its containers and network up with
`^/?k3d-...$`), and `--privileged` gives the container the host's device
nodes (the kubelet reads `/dev/kmsg`).

Without any Docker API, `compat/baseline-k3s-vm.sh` runs the same k3s
version in a throwaway [bench/vm.sh](../bench/vm.sh) VM (Ubuntu 24.04
under qemu, `brew install qemu`) and tunnels its API to `127.0.0.1:26443`;
`--down` deletes the VM. Validated on L0–L1 (50/50,
[compat/results/20261019-131313-k3s-vm-baseline](../compat/results/20261019-131313-k3s-vm-baseline/)).
Trade-offs against k3d: one node instead of two, a second VM next to the
dev VM (another 4 GiB while it runs), a 600 MB cloud image on first use,
boot, k3s install and L0–L1 together took 102 s; but no Docker at all, and the same harness
the multi-node benchmarks use.

## What still needs OrbStack

Nothing in the day-to-day loop. What is left:

- **The very first guest build on a new Mac** (above): some Linux has to
  compile the first static litek8s, until there are releases to download.
- **`bench/dev-bench-mac.sh`**, which benchmarks litek8s against kind and
  k3d *on* the Mac's Docker: OrbStack is what it measures.
- **Not tried against litek8s's Docker API:** `litek8s apiserver
  --runtime docker` on the Mac (the Kubernetes node agent's Docker
  runtime, used before crun) and `litek8s dev load` from a Docker daemon.
  Both only need a Docker API; neither is part of the loop above.

CI is unaffected: GitHub's Ubuntu runners have Docker, and macOS runners
skip the Docker-backed tests.

## Known issues

- **Non-root writes to shared directories under load.** On the dev VM's
  virtiofs share, a directory that is changing fast can be reported as
  owned by root for a moment, so a non-root process in a container gets
  EACCES (a parallel `cargo build -u $(id -u)` hits it within seconds;
  root is not affected). The build scripts run as root and chown their
  output. A fix belongs in the dev VM's share handling.
- **The VM's disk only grows.** Images and deleted data stay allocated in
  the sparse disk file on the Mac (55 GB after the corpus, the k3d reference
  and the builds). `dev/stable.sh reset` (deletes the cluster's data,
  images included) gives it back.
- **Guest builds are long for a one-line change** (~3 min at 4 vCPUs):
  the release profile at `opt-level=3` for the `litek8s` crate.
