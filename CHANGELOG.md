# Changelog

## Unreleased

- The API server publishes the OpenAPI definitions of the built-in types
  (Kubernetes 1.33) next to the CRDs' schemas, in `/openapi/v2` (JSON and
  protobuf) and `/openapi/v3`. Argo CD's `ServerSideApply=true` and
  `ServerSideDiff=true` work (they failed with `unable to resolve
  parseableType for GroupVersionKind`), and `kubectl explain` shows built-in
  types. Compat corpus case `argocd_ssa` (L4).

## 0.2.0

litek8s is now only the Kubernetes distribution. The original Docker-backed
daemon (`litek8s serve`, `litek8s.dev/v1alpha1` manifests, SSH nodes, its
own Raft and WireGuard hub) and the experimental distributed LLM compute are
gone: about 25,000 lines of Rust, all with Kubernetes-native replacements that
shipped before this release.

### Removed commands and their replacements

| removed | use instead |
|---|---|
| `litek8s serve` | `litek8s dev up` for a local cluster, `litek8s apiserver` (or `litek8s install server`) for a real one |
| `litek8s apply`, `get`, `describe`, `delete` | `kubectl apply/get/describe/delete` (standard Deployments, Services, ...) against that cluster |
| `litek8s events` | `kubectl get events` (or `kubectl events`) |
| `litek8s proxy` | `kubectl port-forward`; NodePorts appear on `localhost` with `litek8s dev` |
| `litek8s service add` | a Kubernetes Service (`kubectl expose deployment ...`) |
| `litek8s node add` | `litek8s install agent --ssh user@host --join ... --token ...` ([docs/clusters.md](docs/clusters.md)) |
| `litek8s node cordon/drain/uncordon` | `kubectl cordon/drain/uncordon` |
| `litek8s control-plane join/status` | `litek8s apiserver --cluster-init` / `--server URL --token T` (or `install server` with those flags), `litek8s server list/remove`, `litek8s lb` ([docs/ha.md](docs/ha.md)) |
| `litek8s vpn ...` | the pod overlay every node joins on its own, with relaying for nodes behind NAT ([docs/networking.md](docs/networking.md)) |
| `litek8s infra init` | provision machines with your own tooling (Terraform, cloud-init running `install.sh server\|agent ...`), then `litek8s cluster apply -f cluster.yaml` |
| `litek8s cluster` (Docker-container clusters, removed earlier) | `litek8s cluster apply -f` for machines over SSH, `litek8s dev up --nodes N` for local multi-node clusters |
| `litek8s llm`, `model`, `tensor`, `pipeline` | llama.cpp's RPC backend as a DaemonSet: [examples/llm-rpc](examples/llm-rpc/README.md), [docs/llm-cluster.md](docs/llm-cluster.md) |
| `litek8s agent` without `--join` (the daemon's SSH node agent) | `litek8s agent --join URL --token T` (`--join` is now required) |
| `litek8s logs <deployment>` of the daemon | `litek8s logs <pod regex>` or `kubectl logs` |

### Other changes

- `litek8s top` and `litek8s tree` always show the current kube context
  (or `--context`); they no longer look for a daemon first.
- The global `--socket-path` and `--server` flags are gone (`agent`, `lb`
  and `server` keep their own `--server`).
- The `llm` cargo feature is gone and with it the memmap2, safetensors,
  tokenizers (oniguruma) and half dependencies; the crate has no features
  left, so `--no-default-features` is no longer needed for the release build.
- The release binary is 17 % smaller: 32.2 MiB instead of 38.7 MiB
  (macOS arm64, stripped).
- `cargo test` no longer needs a Docker API (the Docker-driven tests were
  the daemon's).
- The daemon's files can be deleted by hand: `state.json`, `vpn/` and
  `weights/` in the data directory (`~/.local/share/litek8s` on Linux,
  `~/Library/Application Support/dev.litek8s` on macOS) and
  `$XDG_RUNTIME_DIR/litek8s/{daemon,agent}.sock`. Nothing else litek8s
  keeps is affected.
- Removed docs: docs/legacy-daemon.md, docs/legacy-daemon-demo.md,
  docs/llm.md; the v1alpha1 examples and the Terraform scaffolding under
  `infra/`.

## 0.1.2

See the [GitHub release](https://github.com/jairoFernandez/litek8s/releases/tag/v0.1.2).
