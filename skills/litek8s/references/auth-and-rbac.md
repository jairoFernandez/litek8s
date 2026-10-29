# Authentication and RBAC on litek8s servers

Servers (`litek8s apiserver`, `install server`, `cluster apply`) authenticate
clients and enforce RBAC; dev clusters (`litek8s dev`) do not (`AlwaysAllow`
on 127.0.0.1). Full guide: `litek8s://docs/auth.md`.

## Modes (`--authorization-mode`)

- `RBAC` (servers' default): 401 without valid credentials, 403 when denied.
- `RBAC-audit`: decides, logs `RBAC-audit: DENY ...` / `would reject ...`, allows.
- `AlwaysAllow`: no checks (the behavior before this release).

## Identities

- kubectl/Helm/`litek8s ui`: client certificate from the cluster's client CA
  (CN user, O groups). Admin contexts written by `cluster apply` /
  `install --ssh` / `/etc/litek8s/kubeconfig.yaml` = `system:admin` in
  `system:masters`.
- Pods: their service account token (audience checked).
- Nodes and HA peers: the join token (`system:masters`).
- `kubectl auth whoami` and `kubectl auth can-i ... --as=...` answer for real.

## Symptom → fix

| Symptom | Cause | Fix |
|---|---|---|
| `You must be logged in to the server (Unauthorized)`, `whoami` = `system:anonymous` | context made by an earlier litek8s: placeholder token | upgrade the local `litek8s` CLI, re-run `litek8s cluster apply -f <file>` (or `install server --ssh`, or `lkcloud up`); or copy the certificate from `/etc/litek8s/kubeconfig.yaml` into the context (docs/auth.md has the commands); or run the server with `--authorization-mode RBAC-audit` meanwhile |
| context works, then 401 again after `cluster apply` | an older `litek8s` CLI on PATH rewrote the token | upgrade the CLI (lkcloud uses `litek8s` on PATH or `LITEK8S_BIN`) |
| a pod/controller gets `403 Forbidden ... cannot <verb> resource ...` | its service account has no Role for that (worked only because everyone was admin) | add a Role/RoleBinding (or ClusterRole/Binding) for exactly that access; verify with `kubectl auth can-i <verb> <resource> -n <ns> --as=system:serviceaccount:<ns>:<sa>` |
| unsure what will break before enforcing | — | run `RBAC-audit` for a day, then `journalctl -u litek8s-server | grep "RBAC-audit: DENY"` on the server |
| want to see denials in `RBAC` mode | — | `journalctl -u litek8s-server | grep -E "RBAC: forbidden|authentication failed"` (one line per user and kind of access) |
| metrics-server (aggregated API) answers 403 | no front-proxy identity yet | use the built-in `metrics.k8s.io` (delete metrics-server's APIService) |

Cluster-scope bindings to `admin`/`edit` (CI runners, workflow servers) now
really grant reading Secrets in every namespace: suggest RoleBindings instead.
