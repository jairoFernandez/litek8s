# Security policy

## Reporting a vulnerability

Please do **not** open a public issue for security problems. Report them
privately to:

- **Email:** TODO-security-contact@example.com  <!-- TODO(maintainer): replace with a real address -->
- or GitHub's private vulnerability reporting: the repository's
  **Security** tab → **Report a vulnerability** (once enabled in the
  repository settings).

Include what you found, how to reproduce it, and the litek8s version
(`litek8s --version`) and platform. You should get an answer within a
week. litek8s is maintained by one person in their spare time, so please
allow reasonable time for a fix before disclosing.

## Supported versions

Only the latest release gets fixes while litek8s is 0.x.

## Known security limitations

These are design gaps of the current version, not vulnerabilities to
report (but ideas for closing them are welcome):

- **Authentication and RBAC are newer and simpler than upstream's**
  (docs/limitations.md lists what is missing: no OIDC or webhook
  authenticators, no Node authorizer, no client certificate revocation).
  Servers enforce RBAC by default; `--authorization-mode AlwaysAllow` (and
  every `litek8s dev` cluster, which listens on 127.0.0.1 only) makes every
  client that reaches the port cluster admin. Keep port 6443 (and the HA
  Raft port, 6444) on a trusted network or behind a firewall or VPN anyway.
- The admin kubeconfig a server writes (`/etc/litek8s/kubeconfig.yaml`, or
  the context `install --ssh` and `cluster apply` add) holds a
  `system:masters` client certificate valid for ten years; treat it, and
  `<data-dir>/client-ca.key` and `sa.key`, like the join token.
- The Docker API socket (`~/.litek8s/docker.sock`) gives whoever can open it
  the same power as the Docker socket: it is created for your user only.
- Node join and HA peers authenticate with the join token (which also pins
  the cluster CA); anyone holding it can join a node, and as a bearer token
  it is a `system:masters` credential. Treat `<data-dir>/join-token` like a
  password.
- The macOS release binary is ad-hoc signed, not notarized.
