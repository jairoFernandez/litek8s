---
title: "A home lab across NATs: one cluster from a desktop, a cloud VM and a friend's machine"
description: "Three machines behind different NATs, one litek8s cluster. How the WireGuard overlay finds direct paths, when it falls back to a relay, and what that costs."
date: 2026-10-30
tags: [home lab, networking, wireguard, nat, multi-node]
docs: [networking, clusters, multi-node, observability]
translation: es/home-lab-a-traves-de-nats
---

The machines you actually have are rarely on one network. A desktop at
home behind the ISP's router. A cheap cloud VM whose own address is
private, with a public one mapped onto it. A friend who offered the spare
box under their desk, behind a different router in a different city.

Most guides answer that with "put a VPN in front of everything first". We
wanted litek8s to make that one cluster on its own: every pod reaches every
other pod, Services work across all three, and nobody touches a router.
This post is how it does that, what we measured, and the case it cannot
solve.

## The setup

| machine | where | role |
|---|---|---|
| `cloud` | a cheap cloud VM, public address 1:1 NAT onto a private one | server, relay |
| `home-a` | a Linux desktop at home, behind the home router | node |
| `home-b` | a friend's Linux machine, behind their router | node |

All three run Linux (x86_64 or aarch64) with systemd; servers and nodes
need Linux, macOS is for dev clusters. Nothing else is needed on them
beforehand: litek8s, crun and iptables are installed when missing.

## Building it

From a laptop at home, which can SSH into the VM and the desktop, one file
covers the two machines we control:

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                        # the kubeconfig context
joinUrl: https://203.0.113.7:6443   # the VM's public address, not its private one
servers:
  - ssh: ubuntu@203.0.113.7
    name: cloud
    args: [--node-external-ip, 203.0.113.7]
agents:
  - ssh: me@desktop
    name: home-a
```

```bash
litek8s cluster apply -f cluster.yaml           # install / update, idempotent
litek8s cluster status -f cluster.yaml          # service, version, node, readiness, pods per machine
```

Two details matter here. The VM only knows its private address, so
`--node-external-ip` gives it the public one: it becomes the WireGuard
endpoint the node announces and the Node's `ExternalIP`. And `joinUrl`
overrides the default (the first server's node IP), which would be that
private address. In the VM's security group, open UDP 51820 for WireGuard
and TCP 6443 for the API.

The friend does not need to give us SSH. They run the join command the
server printed, which looks like this:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh -s -- \
  agent --join https://203.0.113.7:6443 --token K10<hash>::<secret>
```

That installs the binary, checks the machine (cgroup v2, kernel WireGuard,
free ports) and starts a `litek8s-agent` systemd unit.

## How a node behind NAT gets reached

Every node gets a `/24` of `10.42.0.0/16` and one `litek8s-wg` interface,
with every other node as a WireGuard peer. Pod addresses are never
masqueraded between nodes, so a pod sees the real address of the pod
talking to it. Three mechanisms then deal with NAT:

1. **Keepalive.** Every peer has a 25 s keepalive. A node nobody can reach
   still works: it sends first, and WireGuard on the other side learns
   where its packets come from. That is how `cloud` reaches both homes.
2. **Endpoints seen by others.** Where a node's packets arrive from, as seen
   by the other nodes, is offered as an extra endpoint to try. A home whose
   router forwards the WireGuard port, or maps it the same for every
   destination, gets reached directly this way.
3. **A relay.** A pair with no direct path at all goes through a node both
   reach. By default that is the server: here, `cloud`.

To know which pairs need the relay, every agent sends each peer it routes
directly one small UDP datagram over WireGuard every 10 s. WireGuard answers
a data packet with a keepalive, so a working path carries traffic both
ways. The agent classifies each peer:

| state | meaning |
|---|---|
| `direct` | something arrived in the last 40 s over a session younger than 180 s |
| `unreachable` | watched for 40 s without that |
| `unknown` | watched for less than 40 s; the last known state is kept |

The server stores those reports in the Node objects and computes routes.
A pair is relayed only when one side reports the other `unreachable`, so
pairs that work directly are never relayed. While relayed, the direct entry
stays configured without allowed IPs, and its keepalives keep probing; when
neither side reports `unreachable` any more, the pair goes back to direct.

## Seeing what happened

```bash
kubectl get node home-a -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}'
# {"cloud":"direct","home-b":"via cloud"}
litek8s tree
# ├─ nodes
# │  ├─ Node cloud   Ready, 3 pods, pods 10.42.0.0/24
# │  ├─ Node home-a  Ready, 2 pods, pods 10.42.1.0/24, overlay: home-b via cloud
```

Both homes reach `cloud` directly; between themselves they go through it.
The built-in metrics say how much that costs:

```bash
litek8s query 'rate(litek8s_overlay_relayed_bytes_total[5m])'
litek8s query 'litek8s_overlay_peer_relayed == 1'
```

## What the relay costs

The data path stays in the kernel. On `home-a`, the peer `cloud` gets
`home-b`'s pod CIDR in its allowed IPs; `cloud` decrypts the packet, its
routing table sends it back into `litek8s-wg`, and WireGuard encrypts it
towards `home-b`. No userspace proxy, no extra daemon.

What you pay:

- **The relay's bandwidth.** Every relayed byte enters and leaves `cloud`.
  On a cloud VM, that is its egress bill. `litek8s_overlay_relayed_bytes_total`
  tells you how many bytes it forwarded for others.
- **One extra hop.** Traffic between the homes goes out to the VM and back,
  whatever the distance between them.
- **Time to notice.** A path that breaks is relayed after about a minute
  (40 s without answers, then the next report and push). A path that comes
  back is used again once a handshake gets through on it, which can take up
  to the next WireGuard rekey, 2 to 3 minutes.

The relay is not an open forwarder. It forwards only between WireGuard
peers and only between pod addresses (`LITEK8S-OVERLAY` filter chain), and
the peers are the cluster's own nodes, which joined with the token.

## Measured, not assumed

`bench/nat-e2e.sh` builds this exact shape: three VMs, a server with a
public address and two homes, each home's agent in a network namespace
behind its own NAT (both homes on `10.99.0.0/24`), and the two homes
cannot reach each other. It checks pod-to-pod traffic and Services both ways through the
relay, then adds a port forward on one router, cuts the path and restores
it.

Last run (2026-10-21, qemu + KVM on a Linux host, Ubuntu 24.04 amd64 guests,
2 vCPU each):

| event | time |
|---|---|
| agents started → homes relayed | 44 s |
| port forward added → direct | 27 s |
| direct path cut → traffic back through the relay | 45 s |
| path restored → direct again | 141 s (the next handshake on the direct entry) |

The compat corpus's multi-node (M1) and relay (M2) tiers then ran across
the NATs: 5/5 and 2/2.

## What it cannot do

- **Two strict NATs stay relayed.** There is no hole punching to rely on
  between two NATs that do not let packets in. Linux routers, for one, give
  a flow a new port when an unanswered packet from the other side got there
  first, so neither side ever sees the port it sends to answer. If both
  homes sit behind routers like that, `home-a` ⇄ `home-b` goes through
  `cloud` for good. The fix is a port forward of UDP 51820 on one of the two
  routers; the pair then goes direct with nothing relayed, which is what the
  27 s row above measures.
- **One relay hop.** A pair is relayed only through a node both sides reach
  directly. No chains.
- **IPv4 only**, one WireGuard port per node.
- **Every node must run 0.2 or later.** An older agent never uses a relay.
- **The API port is on the internet.** The homes join over `cloud`'s 6443.
  Servers authenticate clients and enforce RBAC, but there is no OIDC or
  certificate revocation yet, and the docs say to keep 6443 and the Raft
  port off untrusted networks. Restrict the security group to what you can.
- **Your friend's machine holds an admin credential.** The join token is an
  administrator's credential and it lives on every node in
  `/etc/litek8s/agent.env`. Only join machines whose root you would trust
  with the cluster.

## More relays, or none on the server

If the server should not carry other people's traffic, or a second VM
should share it, label nodes:

```bash
kubectl label node my-cloud-vm litek8s.dev/relay=true
kubectl label node my-server litek8s.dev/relay=false
```

A pair's relay is chosen by rendezvous hashing among the relays both sides
reach, so pairs spread over relays and stay put when another relay is added.

## Try it

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Then point a cluster file at your own machines. The whole mechanism, with
the annotations and metrics, is in [Networking across NATs](/docs/networking/);
the SSH install and the cluster file in [Clusters over SSH](/docs/clusters/);
the join command and HA in [Multi-node and HA](/docs/multi-node/).
