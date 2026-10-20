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

- **The Kubernetes API has no client authentication or authorization.**
  Every client that can reach the API port is cluster admin; RBAC objects
  are stored but not enforced. `litek8s dev` clusters listen on 127.0.0.1
  only. On a server, keep port 6443 (and the HA Raft port, 6444) on a
  trusted network or behind a firewall or VPN.
- The Docker API socket (`~/.litek8s/docker.sock`) gives whoever can open it
  the same power as the Docker socket: it is created for your user only.
- Node join and HA peers authenticate with the join token (which also pins
  the cluster CA); anyone holding it can join a node. Treat
  `<data-dir>/join-token` like a password.
- The macOS release binary is ad-hoc signed, not notarized.
