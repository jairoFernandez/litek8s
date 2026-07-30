# litek8s

`litek8s` is a Docker-backed orchestrator demo written in Rust. It keeps the UX close to `kubectl` while staying intentionally small: one binary, a local daemon over a Unix socket, a strict reduced `Deployment` manifest, SSH-backed remote nodes, and a reconciler that recreates containers if they disappear.

## Commands

```bash
cargo run -- serve
cargo run -- apply -f examples/nginx.yaml
cargo run -- apply -f examples/http-echo-service.yaml
cargo run -- get deployments
cargo run -- describe deployment demo-nginx
cargo run -- logs demo-nginx
cargo run -- proxy echo-demo
cargo run -- service add echo-svc --deployment echo-demo --listen 127.0.0.1:9100
cargo run -- get services
cargo run -- events
cargo run -- events --deployment demo-nginx
cargo run -- events --node server77 --watch
cargo run -- top
cargo run -- top --watch
cargo run -- tree
cargo run -- tree --watch
cargo run -- node cordon server77
cargo run -- node uncordon server77
cargo run -- delete deployment demo-nginx
```

`top` shows a daemon summary first, then the managed deployments table.
`events` shows the control-plane history: scheduling, starts, backoff, deletes, and node registration.
`events` can also filter by deployment or node so noisy workloads do not drown the rest of the stream.
`proxy` opens a local TCP listener and forwards into the deployment's published `hostPort`, using SSH tunneling automatically when the workload is on a remote node.
`service add` creates a daemon-managed local listener with a stable address, so you do not need to keep a foreground `proxy` process open.
The same service can also be declared as YAML with a small selector-based `Service` manifest and applied with `litek8s apply -f ...`.
`tree` gives you a real relationship view from the control plane down to nodes, deployments, published ports, and services, so you do not have to reconstruct topology mentally from separate tables.

## Local LLM mmap backend

Local Qwen inference has three interchangeable weight readers:

- the default `resident` backend loads and transposes every projection as
  `f64`;
- `--mmap` keeps large tensors in their native SafeTensors dtype and streams
  them directly from the read-only mapping;
- `--gguf FILE` reads the mixed-quantized GGUF format used by Ollama directly
  through mmap. Q4_K_M is a recipe mixing Q4_K, Q5_0, Q6_K, Q8_0 and F32
  tensors; every supported block is dequantized inside the dot product.

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --mmap \
  --max-new 1 \
  "The capital of France is"
```

The same Ollama blob can be tested without copying or converting it:

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --gguf ~/.ollama/models/blobs/sha256-... \
  --max-new 1 \
  "The capital of France is"
```

Incremental generation can place the KV cache on one or more disks:

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --gguf ~/.ollama/models/blobs/sha256-... \
  --kv-cache-dir /fast-disk/litek8s-kv,/large-disk/litek8s-kv \
  --kv-max-tokens 32768 \
  --kv-buffer-mb 16 \
  --max-new 4 \
  "The capital of France is"
```

This mode performs one batched prompt prefill and then processes only the new
token at each decode step. It creates one grow-on-demand F16 K/V file per
transformer layer, distributes the files round-robin across the supplied
directories, and accesses them through writable mmap. Files grow in 64-token
groups, avoiding full-capacity allocation on filesystems such as APFS. Only the
configured tile buffer and the current attention scores need additional
resident memory. Cache files are removed automatically; pass
`--keep-kv-cache` to retain them for inspection or reuse by external tooling.

For Qwen2.5-0.5B, disk KV storage costs 12 KiB per cached token across all 24
layers. A 32,768-token limit therefore has 384 MiB of maximum capacity, while
the initial mapped allocation is only 1.5 MiB across 24 layers. The CLI reports
both bytes containing live rows and the current mapped file lengths.

The mmap backend loads only requested embedding rows, decodes each projection
weight once per input batch, reuses it across the batch rows, and computes tied
embedding logits without materializing the vocabulary matrix. The original
backend remains the default so both paths can be compared.

Measured on Qwen2.5-0.5B for one generated token:

| backend | wall | maximum RSS | output |
|:---|---:|---:|:---|
| resident `f64` | 90.31 s | 4,750.7 MB | `Paris` |
| native mmap | 4.86 s | 1,071.4 MB | `Paris` |

RSS includes pages from the 942 MiB checkpoint mapping. The mapped backend is
exact at the checkpoint dtype. The GGUF backend instead accepts the model's
Q4_K_M quantization error in exchange for its smaller working set.

An alternating, page-cache-warm comparison isolates the two mmap readers:

| mmap backend | file size | wall (1 token) | maximum RSS | output |
|:---|---:|---:|---:|:---|
| SafeTensors BF16 | 988,097,824 B | 2.86 s | 1,073.5 MB | `Paris` |
| Ollama GGUF Q4_K_M | 397,807,936 B | 2.35 s | 476.3 MB | `Paris` |

The quantized file is 2.484x smaller, reduces measured RSS by about 55.6%, and
is about 1.22x faster in the current scalar CPU kernel. GGUF startup alone is
about 0.40 s versus 0.20 s for SafeTensors.

With the same GGUF, prompt, warm page cache, and four generated tokens, the disk
KV path produced the same `Paris. It is` output in 4.31 s versus 10.18 s for
full-prefix recomputation, a 2.36x speedup. It used 0.10 MiB of KV rows for the
eight positions actually processed. These timings describe the current scalar
CPU implementation; longer contexts trade additional disk I/O for bounded,
evictable memory instead of keeping the whole KV history in anonymous RAM.

Measured with `/usr/bin/time -l` on macOS, both modes peak at about 454 MiB of
RSS — dominated by the file-backed, evictable pages of the ~379 MiB mmapped
GGUF — while the anonymous "peak memory footprint" is 82 MiB for the disk KV
path versus 84 MiB for recomputation. At only eight cached positions the KV
difference is negligible; the disk path's advantage grows with context length,
since its resident KV state stays bounded by the tile buffer rather than the
full history.

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
target/debug/litek8s tree --watch --interval-secs 1
target/debug/litek8s logs logstorm --tail 20
target/debug/litek8s describe deployment oom-demo
target/debug/litek8s proxy echo-demo --listen 127.0.0.1:9000
target/debug/litek8s apply -f examples/http-echo-service.yaml
target/debug/litek8s apply -f examples/http-echo-edge-service.yaml
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

`litek8s` can also place a deployment on another machine over `ssh`. The simplest path is `node add --install`, which detects the remote platform, builds a compatible binary, copies it over SSH, and registers the node.

1. Register the node in the local control plane:

```bash
cargo run -- node add server77 --ssh server77 --install --label role=edge --label zone=lab
cargo run -- get nodes
cargo run -- node cordon server77
cargo run -- node uncordon server77
```

If you already copied the correct binary manually, `--install` is optional.

2. Apply manifests that target that node:

```bash
cargo run -- apply -f examples/http-echo-server77.yaml
cargo run -- apply -f examples/http-echo-edge.yaml
cargo run -- apply -f examples/http-echo-edge-service.yaml
cargo run -- get deployments
cargo run -- describe deployment echo-server77
cargo run -- describe deployment echo-edge
cargo run -- get services
cargo run -- describe service edge-svc
cargo run -- tree --watch
cargo run -- events --node server77 --watch
cargo run -- events --deployment echo-edge
cargo run -- proxy echo-edge --listen 127.0.0.1:9002
cargo run -- top --watch
```

The remote examples cover both scheduling paths:

- `examples/http-echo-server77.yaml` uses `spec.template.spec.nodeName: server77`
- `examples/http-echo-edge.yaml` uses `spec.template.spec.nodeSelector.role=edge`

`get` / `describe` / `top` now show a `NODE` column so you can see where each deployment is running, and `events` lets you watch the scheduler and reconciler decisions in real time.
`get nodes` and `describe node` also show whether a node is `Schedulable` or `Cordoned`, which makes it easy to demo controlled placement.
`tree` is the quickest way to show the actual cluster shape: which deployment lives on which node, and which service currently points at which deployment.
`proxy` is useful when the remote node is only reachable over SSH or when you want one stable local URL during the demo.
`service` is the next step up: the daemon owns the listener, keeps it alive across CLI exits, and forwards each connection to the deployment's current node and published port.
The declarative subset is intentionally small: `kind: Service`, `spec.selector`, and one TCP entry in `spec.ports`. The daemon resolves the selector to exactly one deployment in v1; built-in load balancing is not implemented yet. The older `deploymentName` + `listenAddr` + `targetPort` shape still works for compatibility.

## Manifest shape

The supported manifest is deliberately narrow:

- `apiVersion: litek8s.dev/v1alpha1`
- `kind: Deployment`
- `spec.replicas: 1`
- exactly one container
- `restartPolicy: Always`
- optional `nodeName` and `nodeSelector` in `spec.template.spec`
- optional `metadata.labels` for service selection
- optional `command`, `args`, `env`, `ports`, and `resources.limits`
- supported limits are `cpu` and `memory` with k8s-style strings such as `500m` and `128Mi`

The supported `Service` subset is also deliberately narrow:

- `apiVersion: litek8s.dev/v1alpha1`
- `kind: Service`
- `spec.selector` must match exactly one deployment
- `spec.ports` must contain exactly one TCP port
- `spec.ports[0].port` becomes the local listener port on `127.0.0.1`
- optional `spec.ports[0].targetPort` overrides the deployment's published container port

## Notes

- The daemon uses the local Docker socket directly and supports Linux plus macOS setups such as OrbStack and Docker Desktop.
- Remote nodes are reached over `ssh` and execute the same `litek8s` agent binary against their local Docker daemon.
- No overlay networking, kube-proxy, load balancing, volumes, secrets, or rollouts are included in v1.
