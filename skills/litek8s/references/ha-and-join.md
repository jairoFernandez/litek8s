# HA control plane and joining nodes

These are for real clusters on several machines (home servers, edge boxes,
cheap VMs), not for `litek8s dev` (whose `--nodes` already uses the join
flow inside one VM/sandbox). They change machines: run them only when the
user asks, on the hosts they name.

## A server and nodes

```bash
# server (runs pods too unless --no-node); prints the join token
litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 --kubeconfig ~/.kube/litek8s
# every other machine (Linux, root, crun installed)
litek8s agent --join https://192.168.1.10:6443 --token K10<sha256 of CA>::<secret>
```

- The token is `K10<hash>::<secret>`, in `<data-dir>/join-token`. The agent
  pins the server's CA by that hash, checks a per-node password and gets a
  pod CIDR; pods talk across nodes over a WireGuard overlay (UDP 51820,
  `--wireguard-port`; `--no-wireguard` to skip).
- Nodes behind NAT work as they are: pairs with no direct path are relayed
  by a server (or nodes labeled `litek8s.dev/relay=true`). A cloud VM whose
  own address is private: `--node-external-ip <public>`. Routes per node:
  annotation `litek8s.dev/wireguard-routes` (`direct` / `via <relay>`),
  `litek8s tree`, metrics `litek8s_overlay_*` (docs/networking.md).
- `--node-grace-period 15m` for a node that sleeps (a laptop).
- `--runtime auto|crun|docker` (auto: crun on Linux as root when installed).
- Node heartbeats renew Leases in `kube-node-lease`; a node silent past the
  grace period (40 s) turns NotReady, its pods are evicted after their
  tolerationSeconds.

## 3 or 5 servers (Raft)

```bash
litek8s apiserver --data-dir D --listen 0.0.0.0:6443 --node-ip 192.168.1.10 --cluster-init
litek8s apiserver --data-dir D --listen 0.0.0.0:6443 --node-ip 192.168.1.11 --server https://192.168.1.10:6443 --token K10...
litek8s server list --server http://192.168.1.10:6443
litek8s server remove lk3 --server https://192.168.1.10:6443 --token K10...
litek8s lb --listen 127.0.0.1:6443 --server http://192.168.1.10:6443   # one address for kubectl
```

- Raft runs on the API port + 1. Writes go to the leader (forwarded);
  reads are linearizable after a read barrier; `resourceVersion=0` reads
  may be stale.
- Any one server of 3 (two of 5) can die without losing writes. Remove a
  server before reinstalling it with an empty data directory.
- Agents learn every server and fail over by themselves.
- Slow disks (SD cards) can cause elections; see docs/ha.md "Disks".
- `litek8s ha-check` runs a linearizability workload against the servers.

## Security

Servers authenticate API clients (service account tokens, client
certificates from the cluster's client CA, the join token as admin) and
enforce RBAC (`--authorization-mode RBAC`, the default; `RBAC-audit` logs
and allows; `AlwaysAllow` is the old no-auth mode, which dev clusters
use on 127.0.0.1). The join path (agents) is authenticated with the join
token and the node password.
