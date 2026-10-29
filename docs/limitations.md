# Limitations

Honest list; more in [skills/litek8s/references/differences.md](../skills/litek8s/references/differences.md).

- **Authentication and RBAC are simpler than upstream's.** `litek8s
  apiserver` (and so `install server`, `cluster apply`) authenticates
  clients and enforces RBAC by default (`--authorization-mode RBAC`): service
  account tokens, client certificates signed by the cluster's client CA (CN
  user, O groups; the kubeconfig it writes is `system:admin` in
  `system:masters`), and the join token (an administrator's credential).
  Anonymous requests reach only `/healthz`, `/livez`, `/readyz`, `/version`
  and the OIDC discovery documents; everything else gets 401.
  `--authorization-mode RBAC-audit` decides, logs (`RBAC-audit: DENY ...`,
  and each user's first use of a verb and resource as `ALLOW`) and allows,
  to find the RBAC a cluster's charts lack before enforcing;
  `AlwaysAllow` is the old behavior. Dev clusters (`litek8s dev`) stay
  AlwaysAllow on `127.0.0.1`. Missing next to upstream: OIDC, static token
  and webhook authenticators; the Node and ABAC authorizers (the nodes'
  agents talk to the server over their own authenticated connection);
  client certificate revocation (certificates are valid for ten years; to
  cut one off, rotate the client CA by replacing `<data-dir>/sa.key`, which
  also invalidates every service account token); aggregated API servers
  (metrics-server) get no front-proxy identity, so their delegated
  authorization sees anonymous requests; the escalation check compares rules
  one by one (a role whose verbs are covered by several of the author's
  rules together may be refused). Sign your own users' certificates with
  `<data-dir>/client-ca.{crt,key}` (an Ed25519 CA; `extendedKeyUsage =
  clientAuth`). Keep the API port (and the Raft port, 6444) off untrusted
  networks anyway.
- One person's project, weeks old: APIs, flags and on-disk formats may
  change between 0.x releases without migration.
- Not every kubelet behavior: no `Unhealthy`/`BackOff` events, a memory
  `emptyDir` is one tmpfs per container mount; ValidatingAdmissionPolicy
  and webhook `matchConditions` are accepted but not enforced.
- NetworkPolicies are enforced on crun nodes (Linux) only; the Docker
  runtime (macOS servers) stores them without effect. Traffic from a node to
  its own pods (probes, NodePort and LoadBalancer relays, which run on the
  node) is always allowed, and pods with `NET_ADMIN` can forge source
  addresses on their node's bridge
  ([networking.md](networking.md#networkpolicies)).
- Pods' `imagePullSecrets` are not used: nodes pull with the credentials of
  their registry configuration ([registries.md](registries.md)), and the
  Docker API cannot `docker push` yet.
- Metrics cover containers, nodes and cluster state; application
  `/metrics` endpoints are not scraped (point a Prometheus at them).
- The macOS VM never returns memory to macOS once touched (a
  Virtualization.framework limit); litek8s keeps the guest small instead
  ([dev.md](dev.md#memory-on-macos)).
- macOS servers use Docker for pods and are meant for development; run
  real servers and nodes on Linux.
