# Limitations

Honest list; more in [skills/litek8s/references/differences.md](../skills/litek8s/references/differences.md).

- **No client authentication or RBAC enforcement.** Every API client is
  admin. Dev clusters listen on `127.0.0.1` only; on a server, put the API
  behind a firewall or VPN and do not expose port 6443 (or the Raft port,
  6444) to untrusted networks. Node join and HA peers are authenticated by
  the join token.
- One person's project, weeks old: APIs, flags and on-disk formats may
  change between 0.x releases without migration.
- Not every kubelet behavior: no `Unhealthy`/`BackOff` events, a memory
  `emptyDir` is one tmpfs per container mount; ValidatingAdmissionPolicy
  and webhook `matchConditions` are accepted but not enforced.
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
