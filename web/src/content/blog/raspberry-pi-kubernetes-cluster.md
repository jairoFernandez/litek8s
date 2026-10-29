---
title: "Kubernetes on a Raspberry Pi and an old laptop with litek8s"
description: "Turn a Raspberry Pi, an old laptop and a cheap cloud VM into one Kubernetes cluster with litek8s: one binary per machine, installed over SSH from your laptop."
date: 2026-10-28
updated: 2026-10-29
translation: es/kubernetes-en-raspberry-pi
tags: [home lab, raspberry pi, multi-node]
docs: [clusters, multi-node, networking, platforms]
---

Most home labs start the same way: a Raspberry Pi in a drawer, a laptop
that is too slow for daily use, maybe a cheap VM in the cloud. Each is too
small on its own, and a "real" Kubernetes distribution asks for more than
they have. litek8s was written for exactly this hardware.

## Why litek8s fits small machines

litek8s serves the real Kubernetes API, so `kubectl`, Helm and Argo CD work
unchanged, but the API server, controllers, scheduler, DNS, service proxy
and node agent are **one Rust process**. On a 1 GiB x86_64 VM the whole
control plane idles at **23 MiB**, against 567 MiB for a minimal k3s
([the numbers and how they were measured](/docs/numbers/)). On a Pi with
1–4 GiB of RAM, that difference is what is left for your pods.

Nodes run containers with [crun](https://github.com/containers/crun) and
no container daemon. Every machine needs Linux on x86_64 or aarch64 (so a
64-bit OS on the Pi), systemd, and root or sudo. Nothing else beforehand:
litek8s, crun and iptables are installed when they are missing.

## One machine at a time

From your laptop, with your own `ssh` (your `~/.ssh/config`, keys and
agent apply):

```bash
# the Pi becomes the server; it runs pods too
litek8s install server --ssh pi@192.168.1.10

# the old laptop joins it, with the token the server printed
litek8s install agent --ssh me@old-laptop \
  --join https://192.168.1.10:6443 --token K10<hash>::<secret>
```

The server's context is added to your kubeconfig and made current, with
the cluster's CA checked against the join token, so `kubectl get nodes`
works right away over verified TLS. If sudo asks for a password, litek8s
asks you once per machine and passes it to `sudo -S` over SSH.

Each step is one SSH command whose output you see: probe the machine,
put the binary in place (kept if it is already the right one, otherwise
uploaded or downloaded from the GitHub release and checked against its
`SHA256SUMS`), install packages, then write and start a systemd unit.

## The whole cluster in a file

Once there are three machines, a file is easier than remembering flags:

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                 # the kubeconfig context
servers:
  - ssh: pi@192.168.1.10
agents:
  - ssh: me@old-laptop
    labels: {disk: ssd}
  - ssh: ubuntu@ec2-host
    name: ec2
    nodeIp: 10.0.0.7
    taints: ["burst=true:NoSchedule"]
```

```bash
litek8s cluster apply -f cluster.yaml    # install or update every machine
litek8s cluster status -f cluster.yaml   # service, version, readiness, pods per machine
```

`apply` is idempotent: run it again and it reports
`cluster home is up to date: nothing changed`, without restarting anything.
Change a label and only that label changes. Take a machine out of the file
and run `apply --prune`: it is cordoned, its pods move elsewhere, and
litek8s is uninstalled from it. Labels and taints you set by hand are left
alone.

## Machines in different places

The laptop at home and the VM in the cloud are rarely on the same network.
Pods on different nodes talk over a WireGuard mesh that litek8s sets up by
itself, and when two nodes cannot reach each other directly (two homes
behind NAT, say), their traffic is relayed through a node both can reach.
Give a cloud VM its public address with `--node-external-ip`; the
[networking guide](/docs/networking/) explains the rest.

## What to keep in mind

litek8s is young, and it says so. Since 0.2.12 a server authenticates
clients and enforces RBAC (the context `install --ssh` and `cluster apply`
add carries an admin client certificate), but there is no OIDC login or
certificate revocation yet, so keep port 6443 on a trusted network or
behind a VPN. If your cluster was made with an earlier release, read
[Authentication and RBAC](/docs/auth/) before upgrading. The full list is
in [Limitations](/docs/limitations/).

The SSH install and cluster file are tested end to end on three Ubuntu
24.04 VMs (`bench/cluster-e2e.sh`): apply, a re-apply that restarts
nothing, pods across nodes, `--prune` and delete.
