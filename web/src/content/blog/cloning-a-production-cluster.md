---
title: "Cloning a production cluster onto litek8s: what Kubernetes taught us"
description: "We copied a running production cluster onto litek8s without touching it. Four bugs, two missing pieces of Kubernetes, and what each says about how it works."
date: 2026-10-29
tags: [migration, rbac, networkpolicy, gitops]
docs: [auth, networking, clusters, agents]
translation: es/clonar-un-cluster-de-produccion
---

The question was simple: could a real production cluster (k3s on a cloud
provider, GitOps with Argo CD, Vault and external-secrets, a few product
APIs with their MongoDB, Postgres, Redis and MinIO, n8n, CI runners) run on
litek8s? The only honest way to answer was to build a copy of it, from its
own GitOps repository and last night's backups, and see what broke.

A lot broke. Most of it was not litek8s's fault, and the parts that were
taught us more about Kubernetes than any amount of reading the API
reference.

## Rule one: the copy must not touch production

A copy of a cluster is a copy of its side effects. The same Secrets that let
a backend charge a card, send an email or reply on WhatsApp work just as well
from a second cluster. Before anything ran, we listed what could reach the
outside world and closed each door:

- **Credentials**: Vault was restored from its raft snapshot, then every
  third-party key in the copy's Vault (payments, mail, LLM APIs, messaging,
  cloud storage, the backup bucket, DNS) was overwritten, and so was every
  value holding a URL to a host outside the cluster. That last rule caught
  a database connection string pointing at a managed MongoDB we had
  forgotten about.
- **Scheduled jobs**: every CronJob suspended. One of them ran
  `rclone sync` against the backup bucket and pruned old files: a copy
  running it would have overwritten the real backups.
- **Tokens stored in databases**: workflow engines and multi-tenant apps keep
  their users' OAuth and messaging tokens in their own databases, where
  neutralizing Vault does not reach. Those deployments started at zero
  replicas.
- **Controllers that act on the world**: external-dns, the Terraform
  operator, CI runners and the PR automation were left out.
- **Exposure**: the copy's ingress answered only on the VPN, with hostnames
  like `app.<vpn-ip>.sslip.io` and certificates from the copy's own CA.

The production cluster was only ever read, through a `kubectl` wrapper that
refuses every verb but `get`, `logs` and friends.

## What GitOps does not record

A GitOps repository describes the cluster's steady state, not how it got
there. Rebuilding from it alone found three things no running cluster would
ever show:

- A sync wave holding an `ExternalSecret` came before the wave that installs
  its CRD. On the live cluster the CRD had been there for months; on a fresh
  one the sync stalls, with Argo CD too.
- The external-secrets chart ran with `serviceAccount.create: false`; the
  ServiceAccount it needed had been created once by the installer's
  bootstrap and never written down.
- Vault's Kubernetes auth pointed at the old cluster's public API address,
  so a restored Vault validated tokens against the wrong cluster.

## Bug 1: JSON Patch `replace` and Vault's labels

Vault, in HA mode, labels its own pod (`vault-active`, `vault-sealed`) so a
Service can select the active node. On litek8s the labels never appeared,
the `vault-active` Service had no endpoints, and the UI answered 503. The
log said the pod PATCH was rejected with 422.

Vault sends a JSON Patch with `"op": "replace"` for labels that do not exist
yet. RFC 6902 says that should fail. Kubernetes uses
[evanphx/json-patch](https://github.com/evanphx/json-patch) v4, which lets
`replace` create a member missing from an existing object, and Vault relies
on that. litek8s now does the same: a missing member of an object is set; a
missing parent or an array index out of range is still an error. Kubernetes
compatibility is often the bugs people depend on.

## Bug 2: `kubectl exec` that died after a minute

Restoring a database by piping a dump into `kubectl exec -i` failed after
about sixty seconds with `next reader: i/o timeout`, even with data flowing.

Since 1.30, kubectl's exec runs over WebSocket (`v5.channel.k8s.io`) and
client-go sends a ping every few seconds, closing the stream when no pong
comes back in time. litek8s's WebSocket reader skipped control frames, so
the pings went unanswered. It now answers each ping with a pong, written as
one whole frame under the same lock as the data. A ninety-second silent exec
and a 60 MB upload through stdin pass.

## The real blocker: everyone was admin

Every client that reached a litek8s server was cluster admin, with or
without a token. On a laptop that is convenient. In a cluster running n8n
workflows and CI jobs, any pod could read every Secret, Vault's unseal keys
included. So litek8s servers now [authenticate and authorize](/docs/auth/)
like a Kubernetes API server does:

- **Who is who.** Pods present their service account token; the signature,
  the account, the pod it is bound to and its audience are checked (a token
  minted for Vault is refused). People present a client certificate from the
  cluster's CA: the `CN` is the user, each `O` a group, and `system:masters`
  may do anything. Nodes use their join token.
- **What they may do.** Roles and ClusterRoles, with subresources such as
  `pods/exec`, resource names and wildcards, the default ClusterRoles with
  aggregation, and the escalation check that stops you from granting what
  you do not hold. `kubectl auth can-i` answers for real.
- **How to turn it on without breaking anything.** `RBAC-audit` decides,
  logs `DENY user=... get secrets namespace=...`, and allows anyway. We ran
  the copy in audit mode, found no workload that needed more than its
  charts granted, then switched to `RBAC`. The default service account of a
  product namespace can no longer read Vault's Secrets or create pods in
  `kube-system`; litecd, external-secrets, cert-manager and Vault work
  unchanged, because their charts ship their RBAC.

The lesson for any cluster: a ClusterRoleBinding to `admin` "for the CI
runner" meant nothing while everyone was admin. With RBAC it means the CI
runner can read every Secret in every namespace.

## NetworkPolicy without a CNI plugin

The charts protected their databases with NetworkPolicies ("only the API
and the backup job may reach this Postgres"), and nothing enforced them.
litek8s has no CNI plugin to delegate to, so it does
[the selecting itself](/docs/networking/#networkpolicies): the API server
turns each policy's selectors into pod addresses and ports, sends each node
the rules for its pods, and the node agent writes one iptables chain per
isolated pod. A pod's rules are in place before its first container starts,
so there is no window where a selected pod is open. Replies always pass;
traffic from the node itself (probes, NodePorts) is allowed, as the kubelet
needs.

On the copy, a pod outside a database's allowed clients now gets nothing
from it, while the application it belongs to still connects.

## In 0.2.12

Everything above (the JSON Patch and WebSocket fixes, authentication and
RBAC with the audit mode, NetworkPolicy enforcement) ships in litek8s
0.2.12. Upgrading a server made with an earlier release: read
[Upgrading a cluster made with an earlier litek8s](/docs/auth/#upgrading-a-cluster-made-with-an-earlier-litek8s)
first, since contexts with the old placeholder token get 401.

## What is left

The copy runs. Before it could replace the original: volumes that survive
the loss of a machine (local-path PVs are tied to a node's disk), a load
balancer with a stable address for DNS, image pull credentials per pod
instead of per node, and a web UI for the GitOps controller. And the most
useful outcome may be the copy itself: rebuilt from backups and the GitOps
repository on a schedule, it is a restore drill that proves the backups
work.
