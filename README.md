# litek8s

`litek8s` is a single-node, Docker-backed orchestrator demo written in Rust. It keeps the UX close to `kubectl` while staying intentionally small: one binary, a local daemon over a Unix socket, a strict reduced `Deployment` manifest, and a reconciler that recreates containers if they disappear.

## Commands

```bash
cargo run -- serve
cargo run -- apply -f examples/nginx.yaml
cargo run -- get deployments
cargo run -- describe deployment demo-nginx
cargo run -- logs demo-nginx
cargo run -- delete deployment demo-nginx
```

## Manifest shape

The supported manifest is deliberately narrow:

- `apiVersion: litek8s.dev/v1alpha1`
- `kind: Deployment`
- `spec.replicas: 1`
- exactly one container
- `restartPolicy: Always`
- optional `command`, `args`, `env`, and `ports`

## Notes

- The daemon uses the local Docker socket directly and supports Linux plus macOS setups such as OrbStack and Docker Desktop.
- No multi-node support, overlay networking, services, volumes, secrets, or rollouts are included in v1.
