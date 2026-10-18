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
| user | you are root inside; your `/etc/subuid` range backs the other ids, so image layers keep their owners |
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
  `hostPath: /Users/you/project` volumes work and edits show up live;
- uses the console as the control channel (the guest reports its address,
  `dev down` powers it off cleanly).

The framework requires the `com.apple.security.virtualization` entitlement;
if the binary lacks it, `dev up` runs an ad-hoc signed copy
(`codesign --sign -`), so nothing needs `sudo`. VM size: `--memory` (MiB,
default 2048) and `--cpus` (default half the Mac's).

Build the guest once (needs Docker or any Linux box for the musl build):

```bash
dev/vm/build.sh   # static litek8s, then kernel + initramfs from Alpine packages
```

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
