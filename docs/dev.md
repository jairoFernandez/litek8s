# `litek8s dev`: a local Kubernetes cluster for development

`litek8s dev` is a single-node Kubernetes cluster for your laptop, in the
role kind, k3d, minikube or Docker Desktop's Kubernetes play, without Docker.
It is the same `litek8s apiserver` (API server, controllers, scheduler,
cluster DNS, service proxy) with a node agent on crun, packaged so that:

```bash
litek8s dev up          # ~0.1 s on Linux, ~1.5 s on macOS; merges context litek8s-dev into ~/.kube/config
kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')
litek8s dev load myapp:dev       # an image of your local Docker/Podman, or a `docker save` archive
litek8s dev status               # mode, server, pods, memory, disk
litek8s dev down                 # stop; objects and images are kept
litek8s dev reset                # stop and delete everything, and the kube context
```

Numbers against kind and k3d are in [bench/RESULTS.md](../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d);
the corpus score in [compat/RESULTS.md](../compat/RESULTS.md).

## Linux: a rootless sandbox

The cluster runs in namespaces of its own, as your user:

| namespace | what it isolates |
|---|---|
| user | your uid keeps its number inside; your `/etc/subuid` range backs every other id, root included, so image layers keep their owners and files a container creates in a bind mount as your uid are yours on the host (`LITEK8S_DEV_IDMAP=root` makes you root inside instead, Docker-rootless style) |
| network | the `litek8s0` bridge, pod veths and **every** iptables rule (pod NAT, Service DNAT) |
| mount | a fresh `/run` (pod network namespaces, crun state), sysfs, a cgroup2 mount rooted at the cluster's cgroup |
| PID | PID 1 is a small init; stopping it stops every pod process |
| cgroup, UTS, IPC | the node's cgroup tree, hostname `litek8s-dev` |

Pods reach the Internet through [pasta](https://passt.top) (or
slirp4netns), a userspace network stack on the host side, the way rootless
Podman does. The host side (`litek8s dev supervise`) binds the API port on
`127.0.0.1` before the sandbox exists, and binds NodePorts and LoadBalancer
ports on `127.0.0.1` when the cluster asks for them (it receives the
listening sockets as file descriptors), so `localhost:<nodePort>` works
without kind's `extraPortMappings`.

**What it never touches:** the host's network interfaces, routes and
iptables/nftables rules (Docker's or anyone's), `/run`, other users' files.
Everything it writes is under `~/.local/share/litek8s/dev`.

### What it needs

| need | why | how |
|---|---|---|
| `crun` | runs the containers | `apt install crun` / `dnf install crun` |
| `newuidmap`, `newgidmap` (setuid) | map your subordinate ids into the user namespace | package `uidmap` (Debian/Ubuntu) or `shadow-utils`; usually already installed |
| a subuid/subgid range of 65536 ids | owners inside images | `/etc/subuid`, `/etc/subgid`; `useradd` creates them on current distros |
| `pasta` or `slirp4netns` | pod egress | package `passt` (or `slirp4netns`) |
| `ip`, `iptables-restore` | used inside the sandbox only | `iproute2`, `iptables` (nft or legacy backend) |
| cgroup v2 + a systemd user session | a delegated cgroup for the cluster's resource limits | `dev up` starts the supervisor in a `systemd-run --user --scope -p Delegate=yes` scope |
| Linux ≥ 5.11 | overlayfs in a user namespace (`userxattr`) | |
| **Ubuntu ≥ 23.10 only:** `sudo litek8s dev setup`, once | AppArmor restricts unprivileged user namespaces; this installs `/etc/apparmor.d/litek8s` (an unconfined profile with `userns`, like Ubuntu's own profile for crun) for the litek8s binary's path | re-run if you move the binary |

`litek8s dev setup` (without sudo) checks all of this and says what is
missing. That is the whole list: no daemon, no group membership, no
capabilities on the binary, no setuid litek8s. Run as root, `dev up` uses the
same sandbox minus the user namespace (cgroup `/sys/fs/cgroup/litek8s-dev`).

## macOS: a small VM on Virtualization.framework

`dev up` boots a Linux VM with Apple's Virtualization.framework (the
`litek8s dev-vm` process; no Docker Desktop, OrbStack, Lima or vfkit). The
guest is Alpine's `linux-virt` kernel and an initramfs of crun, iptables,
iproute2, e2fsprogs and busybox around a static litek8s that runs as PID 1;
a sparse disk holds images and objects. The Mac side:

- forwards `127.0.0.1:<port>` to the API server (6443 when free);
- watches the Services and listens on `127.0.0.1:<nodePort>` for every
  NodePort and on `127.0.0.1:<port>` for LoadBalancer ports
  (`EXTERNAL-IP` is `127.0.0.1`, as with Docker Desktop's Kubernetes);
- shares `$HOME` (or `--share <dir>`) over virtiofs at the **same path**, so
  `hostPath: /Users/you/project` volumes work and edits show up live (also
  `/private/tmp` and `/private/var/folders`, where `/tmp` and `$TMPDIR` are);
- passes file changes under bind-mounted directories into the VM (FSEvents
  on the Mac, IN_ATTRIB in the guest), so inotify watchers see Mac edits;
- shares Rosetta when it is installed, so `linux/amd64` images run;
- serves the Docker API on `~/.litek8s/docker.sock` (see [docker.md](docker.md));
- uses the console as the control channel (the guest reports its address,
  `dev down` powers it off cleanly).

The framework requires the `com.apple.security.virtualization` entitlement;
if the binary lacks it, `dev up` runs an ad-hoc signed copy
(`codesign --sign -`), so nothing needs `sudo`. VM size: `--memory` (MiB,
default 4096, an upper bound: the VM's footprint is what it touches) and
`--cpus` (default half the Mac's).

Build the guest once (needs a Docker API or any Linux box for the musl
build; a running `litek8s dev` serves one, so litek8s can build its next
guest itself, see [develop-without-docker.md](develop-without-docker.md)):

```bash
dev/vm/build.sh   # static litek8s, then kernel + initramfs from Alpine packages
```

## Settings

`dev up` takes its defaults from `~/.litek8s/config.yaml` (`$LITEK8S_CONFIG`),
which `litek8s ui` edits ([ui.md](ui.md)); flags win over it:

| setting | flag | macOS | Linux |
|---|---|---|---|
| `cpus` | `--cpus` | the VM's CPUs (default: half the Mac's, at most 8) | a CPU limit on the sandbox (default: none) |
| `memory` (MiB) | `--memory` | the VM's ceiling (default 4096) | a memory limit on the sandbox (default: none) |
| `shares` | `--share` (repeatable) | directories shared at the same path (default `$HOME`) | - (the sandbox sees the host's files) |
| `port` | `--port` | API server port (default 6443, a free one if taken) | same |
| `publish` | `--publish` | `localhost` (default) or `all`: where NodePorts, LoadBalancer ports and `docker run -p` ports listen | same |

The file is read at `up`; `dev.json` records what the cluster started with.

## Images

- `litek8s dev load <image>` exports the image from the local Docker (or
  Podman's Docker API, via `DOCKER_HOST`) and imports it; `litek8s dev load
  file.tar` takes `docker save` / `podman save` archives in Docker or OCI
  layout, including Docker 25+'s multi-platform archives (only this host's
  platform is used). Loaded images are never pulled over, whatever the pull
  policy, so `myapp:latest` works without `imagePullPolicy: IfNotPresent`.
- A registry on your machine's `localhost` works as it is:
  `image: localhost:5000/myapp:1` reaches the host's port 5000 from inside the
  sandbox (pasta maps the gateway to the host's loopback) or the VM (the
  framework's NAT gateway; the registry must listen on all interfaces, which
  `docker run -p 5000:5000 registry:2` does).
  Other plain-HTTP registries: `LITEK8S_INSECURE_REGISTRIES=host:port`.

## Limits

- One node. No authentication on the API (it listens on 127.0.0.1 only on
  Linux; on macOS also on the VM's address on the framework's private NAT
  network).
- hostPorts are not exposed on the host's localhost (NodePorts are); rootless
  cannot bind host ports below 1024.
- macOS: the first boot formats the data disk (~5 s); DHCP from the
  framework's NAT occasionally takes 2–3 s when no other VM has run for a
  while.
