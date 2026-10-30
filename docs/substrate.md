# Agent Substrate on litek8s

[Agent Substrate](https://ate.dev) runs agents that are idle most of the time:
it suspends and resumes them in under 500 ms inside gVisor sandboxes (or
micro-VMs). It runs on litek8s: the gVisor class installs with Substrate's own
`ate-setup` (kind profile) on a crun node, and its counter demo suspends and
resumes with its state. Design of the pieces it needed:
[specs/09-pod-certificates.md](specs/09-pod-certificates.md).

## What was tested

One litek8s server with its local node on crun 1.21, in an Ubuntu 26.04 arm64
OrbStack machine (13 CPUs, cgroup v2, no `/dev/kvm`), Substrate at
`46b6ea90` (k8s.io/api v0.37) built from source:

```bash
# on the node, as root
docker run -d -p 5001:5000 --name kind-registry registry:3   # ate-setup pushes here
iptables -P FORWARD ACCEPT                                   # Docker sets DROP; pods need forwarding
litek8s apiserver --runtime crun --kubeconfig /root/.kube/config &

# atelet pulls actor images through kind-registry:5000 (its kind overlay)
kubectl -n ate-system create service clusterip kind-registry --tcp=5000:5001
#   ... with an Endpoints object pointing at the node's IP:5001

ATE_INSTALL_KIND=true ate-setup deploy ate-system --context litek8s \
  --credential-provider='{"name":"k8s.io"}' \
  --ingress-allowed-spiffe-ids=spiffe://cluster.local/ns/ate-system/sa/atenet-client
ATE_INSTALL_KIND=true ate-setup deploy demo counter --context litek8s
kubectl ate create actor my-counter-1 -a ate-demo-counter --template counter
kubectl ate proxy actor my-counter-1 -a ate-demo-counter     # localhost:8000
```

Every component runs (podcertificate-controller signing for all of them,
ate-api-server, ate-controller, ate-idp-server, atelet, atenet router and
egress, PostgreSQL over TLS with pod certificates, rustfs, the OTel stack),
the counter template takes its golden snapshot, and the actor counts:

| | latency (`curl` total) |
|---|---|
| request to a running actor | 14–21 ms |
| `kubectl ate suspend`, then a request (resume), 5 times | 106–122 ms |
| suspend, delete every worker pod, then a request (restore from the rustfs snapshot onto a new worker) | 107 ms |

The counter kept counting across all of them (memory, file and rootfs
counters). The worker's per-actor cgroups get every controller (`cpuset cpu io
memory pids`) and the actor's `memory.max` (512 MiB here).

## What litek8s needed (all in this branch)

- `certificates.k8s.io` `ClusterTrustBundle` and `PodCertificateRequest` in
  `v1` and `v1beta1`, and pods' `podCertificate` / `clusterTrustBundle`
  projections, with the node doing the kubelet's part. As upstream, only
  requests made by nodes are tied to real pods (`kubectl ate proxy` asks for a
  client certificate with no pod behind it).
- exec over SPDY: `ate-setup` sets up PostgreSQL with client-go's
  `NewSPDYExecutor`.
- `net.ipv4.ip_unprivileged_port_start=0` (and `ping_group_range`) in every
  pod's network namespace, as containerd does: ate-api-server listens on :443
  as a non-root user.
- `fsGroup` gives group read to `defaultMode: 0600` files: PostgreSQL (uid 70)
  reads its certificate.
- `hostPath` `type` (`DirectoryOrCreate`, `FileOrCreate` and the checks) and
  `mountPropagation` (`HostToContainer`, `Bidirectional`).

Already fine: the worker's `drop: [ALL]` plus its 13 capabilities, seccomp and
AppArmor `Unconfined` (litek8s sets no AppArmor profile), a private cgroup
namespace with cgroup2 mounted read-only (the worker remounts it), taints and
tolerations, NetworkPolicies.

## Substrate's assumptions (not litek8s gaps)

- The kind profile of `ate-setup` needs a Docker daemon (`docker info`, and
  `docker buildx` for the Envoy dataplane image) and a registry at
  `localhost:5001` that nodes pull from and pods reach as `kind-registry:5000`.
- Docker on the same host sets the `FORWARD` policy to `DROP`.
- The base manifests mount GKE paths (`/home/kubernetes/bin`,
  `/etc/srv/kubernetes/cri_auth_config.yaml`); the kind overlay drops them.
- Its `sandboxconfig-validation.yaml` ValidatingAdmissionPolicy is stored but
  not enforced by litek8s (Substrate's controllers still validate).

## Not done

- Micro-VMs: no kubelet device-plugin API (`ate.dev/kvm`), no `/dev/kvm` here.
- Node keys for RSA and P-521 pod certificates (Substrate uses P-256).
- The `attest` permission check on writing a signer's ClusterTrustBundle
  (plain RBAC on the resource instead).
- Several nodes, and Substrate's e2e suite.
