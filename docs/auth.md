# Authentication and RBAC

From this release, `litek8s apiserver` (and so `litek8s install server` and
`litek8s cluster apply`) authenticates every client and enforces RBAC, as a
Kubernetes API server does. Before, every client that reached the port was
cluster admin, with or without a token. Dev clusters (`litek8s dev`) are
unchanged: they stay open (`AlwaysAllow`) on 127.0.0.1.

## Modes

`--authorization-mode` on `litek8s apiserver` (and `install server`, and the
`serverArgs` of a cluster file):

| Mode | Unauthenticated / denied requests | Use it for |
|---|---|---|
| `RBAC` (default on servers) | 401 / 403 | normal operation |
| `RBAC-audit` | allowed, logged as `RBAC-audit: would reject ...` / `RBAC-audit: DENY ...` | finding the RBAC a cluster's charts lack before enforcing |
| `AlwaysAllow` | allowed, nothing checked | the old behavior; dev clusters |

## Who is who

- **You (kubectl, Helm, `litek8s ui`)**: a client certificate signed by the
  cluster's client CA. `CN` is the user, each `O` a group. The kubeconfig a
  server writes (`/etc/litek8s/kubeconfig.yaml`) and the context `install
  --ssh` and `cluster apply` add hold `system:admin` in `system:masters`
  (allowed everything), valid ten years.
- **Pods**: their service account token (signature, account, bound pod and
  audience checked, as TokenReview does). A token for another audience (one
  minted for Vault) is refused.
- **Nodes and HA peers**: the join token, a `system:masters` credential.
- **Impersonation**: `Impersonate-User` / `-Group` headers, allowed with the
  `impersonate` verb (`kubectl --as`).
- **Anonymous**: only `/healthz`, `/livez`, `/readyz`, `/version`, `/cacerts`
  and the OIDC discovery documents; everything else is 401.

Default ClusterRoles (cluster-admin, admin/edit/view with aggregation,
system:discovery, system:basic-user, system:public-info-viewer,
system:auth-delegator, system:monitoring, …) and their bindings are created on
new and existing clusters. `kubectl auth can-i` and `kubectl auth whoami`
answer for real.

## Upgrading a cluster made with an earlier litek8s

A server upgraded to this release starts in `RBAC` mode. Two things can
break:

### 1. Your kubeconfig gets 401 (`You must be logged in to the server`)

The contexts earlier releases added carry a placeholder token, which is now
refused. Check:

```bash
kubectl --context <name> auth whoami
```

Fix it with any of these:

- **Re-run the tool that made the context, with this release's CLI** — it
  puts the admin client certificate in place of the token:

  ```bash
  litek8s cluster apply -f cluster.yaml        # clusters from a cluster file (lkcloud too)
  litek8s install server --ssh root@host ...   # a server installed over SSH
  ```

  An older `litek8s` CLI writes the placeholder token again, so upgrade the
  CLI on your machine (`brew upgrade litek8s`, or the install script) before
  re-running it. lkcloud calls the `litek8s` on your `PATH` (or
  `LITEK8S_BIN`), so the same applies to `lkcloud up`.

- **Copy the server's own kubeconfig** (already holds the certificate; its
  server address is `127.0.0.1`, so keep your context's address):

  ```bash
  ssh root@<server> cat /etc/litek8s/kubeconfig.yaml > /tmp/admin.yaml
  user=$(kubectl config view -o jsonpath='{.contexts[?(@.name=="<name>")].context.user}')
  KUBECONFIG=/tmp/admin.yaml kubectl config view --raw -o jsonpath='{.users[0].user.client-certificate-data}' | base64 -d > /tmp/admin.crt
  KUBECONFIG=/tmp/admin.yaml kubectl config view --raw -o jsonpath='{.users[0].user.client-key-data}' | base64 -d > /tmp/admin.key
  kubectl config set-credentials "$user" --client-certificate=/tmp/admin.crt --client-key=/tmp/admin.key --embed-certs=true
  kubectl config unset "users.$user.token"
  rm /tmp/admin.yaml /tmp/admin.crt /tmp/admin.key
  kubectl --context <name> auth whoami          # system:admin, system:masters
  ```

- **Buy time**: start the server with `--authorization-mode RBAC-audit` (or
  `AlwaysAllow`) until the contexts are fixed.

### 2. Workloads that relied on being admin get 403

Charts, operators and jobs that call the API without shipping their own
Role/ClusterRole worked only because everyone was admin. Typical cases: a pod
using the namespace's `default` service account (ad-hoc `kubectl` Jobs, Helm
hooks, n8n or CI workflows calling the API), CI runners creating Jobs or
exec'ing into pods, workflow engines writing their own CRs.

1. Run the server in audit mode for a while (a day covers most CronJobs):

   ```yaml
   # cluster file (litek8s cluster apply), or lkcloud's `litek8s.serverArgs`
   serverArgs: [--authorization-mode, RBAC-audit]
   ```

2. Read what would be denied, one line per user and kind of access:

   ```bash
   ssh root@<server> 'journalctl -u litek8s-server --since -24h | grep "RBAC-audit: DENY"'
   ```

   `RBAC-audit: would reject ...` lines are clients without valid credentials
   (an old kubeconfig, a token for another audience).

3. Give each of those service accounts a Role/RoleBinding (or
   ClusterRole/ClusterRoleBinding) for exactly what it did, in the chart or
   your GitOps repository, and check:

   ```bash
   kubectl auth can-i create jobs -n ci --as=system:serviceaccount:ci:runner
   ```

4. Switch to `RBAC` (`serverArgs: [--authorization-mode, RBAC]`, then
   `litek8s cluster apply`). Denials are still logged, once per user and
   kind of access:

   ```bash
   ssh root@<server> 'journalctl -u litek8s-server | grep -E "RBAC: forbidden|authentication failed"'
   ```

Workloads that already ship RBAC (cert-manager, external-secrets, ingress-nginx,
Vault's chart with `system:auth-delegator`, Argo CD or litecd's install) work
unchanged. ClusterRoleBindings to `admin` or `edit` at cluster scope, harmless
before, now grant exactly that: reading Secrets in every namespace. Narrow
them to RoleBindings in the namespaces that need them.

## Your own users

Sign certificates with the client CA, on the server (an Ed25519 CA, OpenSSL 3):

```bash
cd /var/lib/litek8s/apiserver            # the server's --data-dir
openssl genpkey -algorithm ed25519 -out jane.key
openssl req -new -key jane.key -subj "/CN=jane/O=dev-team" -out jane.csr
openssl x509 -req -in jane.csr -CA client-ca.crt -CAkey client-ca.key -CAcreateserial \
  -days 365 -extfile <(printf "extendedKeyUsage=clientAuth") -out jane.crt
```

Then bind `jane` (or the group `dev-team`) to a role, for example
`kubectl create rolebinding jane-edit -n team --clusterrole=edit --user=jane`.
There is no revocation: to cut every certificate off, replace
`<data-dir>/sa.key` (which also invalidates every service account token).

## Not there yet

OIDC, static token and webhook authenticators; the Node and ABAC authorizers;
certificate revocation; front-proxy identity for aggregated API servers
(metrics-server's own authorization sees anonymous requests). See
[limitations.md](limitations.md).
