# How litek8s differs from upstream Kubernetes

The API is upstream's (v1.33 objects, discovery, server-side apply,
strategic merge patch, watch, Tables, CRDs with schemas and CEL, admission
webhooks, API aggregation). Conformance: 413/419 of the v1.33 suite on
macOS, the rest pass on Linux; the compatibility corpus (kubectl, Helm with
47 charts) passes 119/119. Assume upstream behavior unless listed here.

## Architecture

- One binary and process: API server, controllers, scheduler, cluster DNS,
  service proxy and (by default) a node agent. No etcd: an in-memory store
  with a write-ahead log, or Raft across 3/5 servers.
- `/version` gitVersion `v1.33.0+litek8s`.
- Node agent instead of kubelet: crun directly (Linux) or Docker (macOS
  development); serves `/metrics/resource` on :10250, so `kubectl top`
  works.

## Missing or partial

- **Authentication and RBAC**: servers (`litek8s apiserver`, `install
  server`, `cluster apply`) default to `--authorization-mode RBAC`: service
  account tokens, client certificates from the cluster's client CA (CN
  user, O groups; the written kubeconfig is `system:admin` in
  `system:masters`) and the join token (admin) authenticate; anonymous
  requests reach only `/healthz`, `/livez`, `/readyz`, `/version` and the
  OIDC documents (401 elsewhere). Role/ClusterRole/bindings are enforced
  (verbs, groups, resources, subresources such as `pods/exec`,
  resourceNames, nonResourceURLs, `*`), with default ClusterRoles
  (cluster-admin, admin, edit, view aggregated, system:discovery,
  system:basic-user, system:public-info-viewer, system:auth-delegator),
  aggregationRule, escalation checks (`escalate`, `bind`), impersonation and
  real SubjectAccessReview/SelfSubjectAccessReview/SelfSubjectRulesReview/
  SelfSubjectReview answers. `RBAC-audit` logs denials (`RBAC-audit: DENY`)
  and allows anyway; `AlwaysAllow` is the old no-auth mode. **Dev clusters
  stay AlwaysAllow** (127.0.0.1 only). Charts that relied on every pod being
  admin need their RBAC: check the server log in RBAC-audit first. Not
  there: OIDC/static/webhook authenticators, the Node authorizer, client
  certificate revocation, front-proxy identity for aggregated APIs.
- **ValidatingAdmissionPolicy** rules and webhook `matchConditions` are
  accepted but not enforced. Writes made by the in-process controllers do
  not go through admission webhooks.
- **Events**: the node agent records Pulled/Created/Started, not kubelet's
  `Unhealthy` (probe failures), `BackOff` or `Failed` (pull errors). Read
  container states instead (`diagnose_pod` does): waiting reasons and
  messages, last termination, restart counts.
- **CRDs**: runtime CEL cost limits are not enforced (the static estimate
  at creation is); timezone arguments accept UTC and fixed offsets only;
  stored objects are not migrated between versions (as upstream).
- A memory `emptyDir` is one tmpfs per container mount (upstream shares one
  per pod).
- Metrics: no scraping of application `/metrics` endpoints, no recording
  rules, no subqueries.
- macOS (Docker runtime) cannot serve DNS over TCP on port 53 or hostPort
  bindings to the node address (the 5 conformance specs it fails).
- hostPorts are not exposed on the host's localhost in dev clusters
  (NodePorts and LoadBalancer ports are).
- LoadBalancer Services: no external load balancer; every server and node
  listens on their ports (like k3s's servicelb), status points at a node;
  `--disable-loadbalancer-ports` keeps only the NodePorts.

## Extras upstream does not have

- `litek8s dev`: many local clusters in one VM/sandbox, ~1-2 s each.
- Docker Engine API on the dev cluster, sharing the image store with pods.
- Built-in metrics (PromQL), log search over all pods including deleted
  ones, alerts with silences, AlertRule/AlertReceiver CRDs.
- `/litek8s/v1/memory`, `/litek8s/v1/cpu`, `/litek8s/v1/disk`: where a
  node's resources go.
- `litek8s ui`: terminal UI (cluster, Docker, observability).
