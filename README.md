# litek8s

`litek8s` is a single-node, Docker-backed orchestrator demo written in Rust. It keeps the UX close to `kubectl` while staying intentionally small: one binary, a local daemon over a Unix socket, a strict reduced `Deployment` manifest, and a reconciler that recreates containers if they disappear.

## Commands

```bash
cargo run -- serve
cargo run -- apply -f examples/nginx.yaml
cargo run -- get deployments
cargo run -- describe deployment demo-nginx
cargo run -- logs demo-nginx
cargo run -- top
cargo run -- top --watch
cargo run -- delete deployment demo-nginx
```

`top` shows a daemon summary first, then the managed deployments table.

## Firepack

If you want a busier demo, these examples are designed to run together:

- [`examples/nginx.yaml`](examples/nginx.yaml): small web server on `localhost:8089`
- [`examples/http-echo.yaml`](examples/http-echo.yaml): tiny HTTP responder on `localhost:8090`
- [`examples/cpu-burn.yaml`](examples/cpu-burn.yaml): synthetic CPU pressure
- [`examples/logstorm.yaml`](examples/logstorm.yaml): noisy container for `logs`
- [`examples/oom-demo.yaml`](examples/oom-demo.yaml): intentional crash loop to exercise backoff

```bash
cargo run -- apply -f examples/nginx.yaml
cargo run -- apply -f examples/http-echo.yaml
cargo run -- apply -f examples/cpu-burn.yaml
cargo run -- apply -f examples/logstorm.yaml
cargo run -- apply -f examples/oom-demo.yaml

target/debug/litek8s top --watch --interval-secs 1
target/debug/litek8s logs logstorm --tail 20
target/debug/litek8s describe deployment oom-demo
```

To clean up:

```bash
cargo run -- delete deployment demo-nginx
cargo run -- delete deployment echo-demo
cargo run -- delete deployment cpu-burn
cargo run -- delete deployment logstorm
cargo run -- delete deployment oom-demo
```

## Remote node demo

`litek8s` can also place a deployment on another machine over `ssh`. In this v1, the remote machine must already have the same `litek8s` binary available.

1. Build the binary locally:

```bash
cargo build
```

2. Copy it to the remote machine and make it executable:

```bash
scp target/debug/litek8s server77:~/.local/bin/litek8s
ssh server77 chmod +x ~/.local/bin/litek8s
```

3. Register the node in the local control plane:

```bash
cargo run -- node add server77 --ssh server77 --install
cargo run -- get nodes
```

If you already copied the correct binary manually, `--install` is optional.

4. Apply a manifest that targets that node:

```bash
cargo run -- apply -f examples/http-echo-server77.yaml
cargo run -- get deployments
cargo run -- describe deployment echo-server77
cargo run -- top --watch
```

The remote manifest uses `spec.template.spec.nodeName: server77`, and `get` / `describe` / `top` will now show a `NODE` column so you can see where each deployment is running.

## Manifest shape

The supported manifest is deliberately narrow:

- `apiVersion: litek8s.dev/v1alpha1`
- `kind: Deployment`
- `spec.replicas: 1`
- exactly one container
- `restartPolicy: Always`
- optional `nodeName` in `spec.template.spec`
- optional `command`, `args`, `env`, `ports`, and `resources.limits`
- supported limits are `cpu` and `memory` with k8s-style strings such as `500m` and `128Mi`

## Notes

- The daemon uses the local Docker socket directly and supports Linux plus macOS setups such as OrbStack and Docker Desktop.
- No multi-node support, overlay networking, services, volumes, secrets, or rollouts are included in v1.
