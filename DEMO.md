# litek8s Demo Guide

`litek8s` is a small Docker-backed orchestrator demo. The idea is simple: one local control plane, familiar `kubectl`-style commands, a reduced `Deployment` manifest, and enough observability to show scheduling, limits, restarts, and a remote node.

## 1. Start the control plane

Run the local daemon in one terminal:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- serve
```

This process is the local control loop. It stores desired state, reconciles deployments, and exposes the CLI API over a Unix socket.

## 2. Register the remote node

In another terminal, bootstrap and register `server77`:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- node add server77 --ssh server77 --install --label role=edge --label zone=lab
cargo run -- get nodes
```

What this does:

- detects the remote OS and architecture
- builds a compatible `litek8s` binary
- copies it over SSH
- validates the remote Docker connection
- registers the node with labels

Expected result:

- `local` appears as `Ready`
- `server77` appears as `Ready`
- `server77` shows labels `role=edge, zone=lab`

## 3. Launch the demo workloads

Apply the local workloads first:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- apply -f examples/nginx.yaml
cargo run -- apply -f examples/http-echo.yaml
cargo run -- apply -f examples/cpu-burn.yaml
cargo run -- apply -f examples/logstorm.yaml
cargo run -- apply -f examples/oom-demo.yaml
```

Then apply the remote workloads:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- apply -f examples/http-echo-server77.yaml
cargo run -- apply -f examples/http-echo-edge.yaml
```

What each workload demonstrates:

- `demo-nginx`: a small web server on `localhost:8089`
- `echo-demo`: a tiny HTTP service on `localhost:8090`
- `cpu-burn`: a container that burns CPU so `top` looks alive
- `logstorm`: a noisy container for log streaming
- `oom-demo`: an intentional memory crash loop to show limits and restart backoff
- `echo-server77`: explicit placement on `server77` with `nodeName`
- `echo-edge`: placement on `server77` via `nodeSelector: role=edge`

## 4. Observe the system

Check the current state:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- get deployments
cargo run -- get nodes
```

Open the live view:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
target/debug/litek8s top --watch --interval-secs 1
```

What to look for:

- daemon CPU and memory at the top
- `NODE` showing whether a deployment is local or remote
- `oom-demo` moving between `Running` and `Restarting`
- `RESTARTS`, `STREAK`, and `NEXT RETRY` increasing when `oom-demo` hits OOM

Watch the control-plane history:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- events --watch
cargo run -- events --deployment echo-edge
cargo run -- events --node server77 --watch
```

This is useful to show scheduling, starts, backoff, restart, and node registration.

## 5. Inspect a few specific workloads

Remote placement by selector:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- describe deployment echo-edge
cargo run -- describe node server77
```

Expected points:

- `echo-edge` should show `node selector: role=edge`
- `echo-edge` should be assigned to `server77`
- `server77` should show labels `role=edge, zone=lab`

Controlled scheduling:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- node cordon server77
cargo run -- get nodes
cargo run -- node uncordon server77
```

Expected points:

- `get nodes` should show `server77` as `Cordoned` after the first command
- selector-based new placements should avoid cordoned nodes
- existing workloads already assigned to `server77` should remain there

OOM and backoff behavior:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- describe deployment oom-demo
```

Expected points:

- memory limit should be `64Mi`
- `last exit` should often be `oom(137)`
- `next retry` should show the restart backoff

Logs:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- logs logstorm --tail 20
```

HTTP checks:

```bash
curl http://localhost:8089
curl http://localhost:8090
```

If `server77` exposes the remote ports to a reachable interface, you can also test the remote services directly from that host or over your LAN.

Local proxy access:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- proxy echo-demo --listen 127.0.0.1:9000
```

Then in another terminal:

```bash
curl http://127.0.0.1:9000
```

For the remote edge deployment:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- proxy echo-edge --listen 127.0.0.1:9002
```

Then:

```bash
curl http://127.0.0.1:9002
```

This proxy is intentionally simple:

- it uses the deployment's published `hostPort`
- local deployments connect directly to `127.0.0.1:<hostPort>`
- remote SSH nodes are reached through an `ssh -W` tunnel per connection
- it is TCP-level forwarding, so it also works for simple non-HTTP TCP demos

Daemon-managed service:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- service add echo-svc --deployment echo-demo --listen 127.0.0.1:9100
cargo run -- get services
cargo run -- describe service echo-svc
```

Then:

```bash
curl http://127.0.0.1:9100
```

For the remote edge deployment:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- service add edge-svc --deployment echo-edge --listen 127.0.0.1:9102
cargo run -- describe service edge-svc
curl http://127.0.0.1:9102
```

This `service` layer is intentionally small:

- it can be configured by CLI or by a small declarative `Service` manifest
- the daemon owns the listener, so it survives after the CLI command exits
- each incoming connection resolves the deployment's current node and published port
- it gives you a stable local entrypoint without adding kube-proxy or an ingress stack
- the selector must resolve to exactly one deployment in v1; there is no built-in service load balancing yet

Declarative service YAML:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- apply -f examples/http-echo-service.yaml
cargo run -- get services
cargo run -- describe service echo-svc
curl http://127.0.0.1:9100
```

And for the remote edge deployment:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- apply -f examples/http-echo-edge-service.yaml
cargo run -- describe service edge-svc
curl http://127.0.0.1:9102
```

The preferred declarative shape is intentionally tiny and k8s-like:

```yaml
apiVersion: litek8s.dev/v1alpha1
kind: Service
metadata:
  name: edge-svc
spec:
  selector:
    app: echo-edge
  ports:
    - port: 9102
      targetPort: 5678
```

What this means:

- `selector` matches a deployment by labels such as `metadata.labels.app: echo-edge`
- `port` becomes the stable local listener, here `127.0.0.1:9102`
- `targetPort` is optional; if omitted, `litek8s` uses the deployment's published TCP port
- the older `deploymentName` + `listenAddr` form still works, but `selector` + `ports` is the preferred demo path now

## 6. Suggested demo script

If you want a short verbal flow while presenting:

1. Start `serve` and explain that the control plane is one local Rust process.
2. Show `get nodes` and explain that `server77` is added over SSH, not with a heavy cluster stack.
3. Apply `echo-edge` and show that scheduling happens through node labels, not just hard-coded node names.
4. Open `top --watch` and point out daemon overhead, workload CPU pressure, and the `oom-demo` backoff loop.
5. Open `events --node server77 --watch` or `events --deployment echo-edge` and show that the control plane is emitting readable lifecycle events without `oom-demo` noise.
6. Run `proxy echo-edge --listen 127.0.0.1:9002` and `curl http://127.0.0.1:9002` to show raw remote access without a Service or ingress stack.
7. Run `apply -f examples/http-echo-edge-service.yaml` and `curl http://127.0.0.1:9102` to show the next level up: a declarative stable entrypoint owned by the daemon and selected by labels.
8. Finish with `describe deployment echo-edge`, `describe service edge-svc`, and `describe deployment oom-demo` to connect scheduling, access, and resource limits.

## 7. Cleanup

Delete the deployments when you are done:

```bash
cd "/Users/civojairo/Documents/New project/litek8s-demo"
cargo run -- delete deployment demo-nginx
cargo run -- delete deployment echo-demo
cargo run -- delete deployment cpu-burn
cargo run -- delete deployment logstorm
cargo run -- delete deployment oom-demo
cargo run -- delete deployment echo-server77
cargo run -- delete deployment echo-edge
```
