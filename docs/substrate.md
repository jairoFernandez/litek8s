# Agent Substrate on litek8s

[Agent Substrate](https://ate.dev) runs agents that are idle most of the time:
it suspends and resumes them in under 500 ms inside gVisor sandboxes (or
micro-VMs). This page tracks what litek8s needs to run it, as checked against
Substrate's manifests and code (`manifests/ate-install/`,
`cmd/atecontroller/internal/controllers/workerpool_apply.go`,
`internal/ateworker/cgroup/`, `internal/deviceplugin/`; k8s.io/api v0.37).
Design: [specs/09-pod-certificates.md](specs/09-pod-certificates.md).

## Done: pod certificates and trust bundles

Every Substrate component authenticates with certificates its
`podcertcontroller` signs. litek8s now serves what that needs:

- `certificates.k8s.io` `ClusterTrustBundle` and `PodCertificateRequest`, in
  `v1` and `v1beta1`, validated like Kubernetes 1.37 (signer names, trust
  bundle PEM, CSR signatures, status transitions and certificate checks), and
  a request must match its pod (UID, service account, node).
- Pods with `projected` `podCertificate` and `clusterTrustBundle` sources.
- Nodes do what the kubelet does: generate the key (ECDSA P-256/P-384 or
  Ed25519), create the request, keep the pod in `ContainerCreating` until the
  signer issues it, write `credentialBundlePath` / `keyPath` /
  `certificateChainPath`, and ask again from `beginRefreshAt`; trust bundles are
  written as deduplicated PEM and follow ClusterTrustBundle changes.

Not done: RSA and P-521 keys on nodes (the API accepts them; Substrate uses
P-256), the `attest` permission check on writing a signer's ClusterTrustBundle
(plain RBAC on the resource instead), the `user` field of projections.

## The gVisor worker on crun nodes (Linux)

The worker pod (`buildDeploymentApplyConfig`) runs as root, unprivileged, with
`capabilities: {drop: [ALL], add: [NET_ADMIN, SYS_ADMIN, SYS_CHROOT,
SYS_PTRACE, SETUID, SETGID, SETPCAP, SETFCAP, DAC_OVERRIDE, FOWNER, CHOWN,
MKNOD, NET_RAW]}`, `seccompProfile: Unconfined`, `appArmorProfile:
Unconfined`, a `hostPath` `/var/lib/ate` (`DirectoryOrCreate`) mounted with
`mountPropagation: HostToContainer`, and tolerates
`ate.dev/sandboxClass=gvisor:NoSchedule`. runsc runs inside the worker; there is
no RuntimeClass or containerd shim to install. atelet (DaemonSet) adds
`CHOWN, DAC_OVERRIDE, FOWNER, FSETID` and hostPaths `/var/lib/ate`,
`/var/lib/kubelet/plugins`, `/var/lib/kubelet/device-plugins`, `/dev` (read
only, at `/host/dev`).

| Need | litek8s (crun) | State |
|------|----------------|-------|
| `drop: [ALL]` + the add list | `capabilities()` in `crun/spec.rs` | ok |
| `seccompProfile: Unconfined` | no seccomp filter when asked | ok |
| `appArmorProfile: Unconfined` | litek8s sets no AppArmor profile at all, so every pod is unconfined | ok (accepted, nothing to do) |
| Private cgroup namespace (`/proc/self/cgroup` reads `0::/`; the worker refuses to run otherwise) | every container gets a cgroup namespace | ok |
| Remount `/sys/fs/cgroup` read-write and nest cgroups (bind-remount with `CAP_SYS_ADMIN`, move itself to a `worker` leaf, enable the delegated controllers) | cgroup2 mounted `ro` for unprivileged containers, as containerd does; nothing blocks the remount | ok, **to verify on a node**: the controllers the worker can enable are those crun enabled in the pod cgroup's `subtree_control`; without `cpu`/`memory`/`pids` there, actors run without their limits |
| `hostPath` `type: DirectoryOrCreate` / `FileOrCreate` | the type is ignored; a missing source path makes the container fail | **gap** (small): create the directory or empty file before the bind; `/var/lib/ate` and `/var/lib/kubelet/{plugins,device-plugins}` do not exist on a litek8s node |
| `mountPropagation: HostToContainer` | bind mounts are always `rprivate` | **gap** (small): `rslave` for HostToContainer, `rshared` for Bidirectional (privileged only). Needed by atelet and the object-store plugin to see disks or CSI volumes mounted under `/var/lib/ate` after they start; a plain `/var/lib/ate` directory works without it |
| `/dev` hostPath read-only | bind mount; the device cgroup still denies opening nodes | ok |
| Device plugins (`/var/lib/kubelet/device-plugins/kubelet.sock`) | no kubelet device-plugin API | gap, **only for micro-VMs** (`ate.dev/kvm` extended resource). atelet retries registration every 5 s and logs; gVisor pools never ask for it |
| `/dev/kvm`, `/dev/net/tun`, Kata | — | not needed for gVisor; out of scope |
| CSI (`/var/lib/kubelet/plugins`) | no CSI | gap only if a SandboxConfig uses CSI-backed durable dirs |
| Image credential provider (`/home/kubernetes/bin`, GKE paths) | — | use the kind overlay, which removes these mounts |
| Taints / tolerations | supported | ok: taint the gVisor nodes `ate.dev/sandboxClass=gvisor:NoSchedule` |
| ValidatingAdmissionPolicy (`sandboxconfig-validation.yaml`) | stored, not enforced | gap: bad SandboxConfigs are not rejected at admission (Substrate's controllers still validate) |
| NetworkPolicies (`ate-system-networkpolicies.yaml`) | enforced by crun nodes | ok |

Also worth knowing:

- runsc itself needs `pivot_root`, `mount`, new network/mount/PID namespaces:
  covered by `SYS_ADMIN` + seccomp `Unconfined` + no AppArmor.
- The macOS dev VM runs nodes as cells inside one Linux VM; gVisor there runs
  nested inside the cell's namespaces. Prefer real Linux nodes (`litek8s
  install agent`) for workers.
- Substrate's install path for kind (`hack/create-kind-cluster.sh`) turns on
  the PodCertificateRequest feature gates; litek8s needs no flags.

## Next steps

1. `hostPath` `type` (`DirectoryOrCreate`, `FileOrCreate`, and the checks of
   the other types) and `mountPropagation` in the crun runtime.
2. Install Substrate with its kind overlay on a cluster of Linux crun nodes and
   check the cgroup delegation of the worker.
3. Micro-VMs: the kubelet device-plugin API (registration socket, `ListAndWatch`,
   `Allocate` into the container's device list and cgroup rules), then
   `/dev/kvm` scheduling.
