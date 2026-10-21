# One LLM across several machines

A model needs its weights in memory. When no single machine you own has
enough, litek8s can pool several: a desktop, a laptop, a NAS, a cheap
cloud VM. [llama.cpp](https://github.com/ggml-org/llama.cpp)'s RPC backend
does the splitting; litek8s runs the pieces, finds them, restarts them and
shows what they use. The manifests are in
[`examples/llm-rpc/`](../examples/llm-rpc/README.md).

What to expect, measured below: a model that fits on no node runs on
three (Qwen2.5 7B, 4.4 GiB of weights, on three 4 GiB machines), and it
answers more slowly than one machine that could hold it alone. Every token
passes through every worker in turn and crosses the network, so pooling
buys memory, not speed.

## How it works

```text
 OpenAI client ──▶ Service llama (NodePort) ──▶ llama-server (Deployment, model on a PVC)
                                                    │ --rpc ipA:50052,ipB:50052,ipC:50052
                     headless Service rpc-server ┄┄┄┤ (ready pod IPs, watched)
                                                    ├──▶ rpc-server on node A ─┐
                                                    ├──▶ rpc-server on node B  ├ DaemonSet,
                                                    └──▶ rpc-server on node C ─┘ layers in RAM
```

- **`rpc-server`**, a DaemonSet on every node labeled
  `llm.litek8s.dev/rpc=true` (and tolerating the taint
  `llm.litek8s.dev/dedicated`, for nodes kept for this job). Each one
  offers its node's CPU and memory (or its GPU with a GPU build of
  llama.cpp) on its pod IP, port 50052. No host network, no host port:
  between nodes the traffic rides litek8s's WireGuard overlay, which
  matters because the RPC protocol has no authentication. `--cache` keeps
  the tensors a worker received on its node's disk
  (`/var/cache/litek8s-llm-rpc`), so a restarted worker reads them locally
  instead of over the network.
- **`llama-server`**, a Deployment of one. Init containers fetch llama.cpp
  and the model (once, onto a PersistentVolumeClaim; litek8s's `local-path`
  class keeps it on the node the server first lands on). Its start script
  asks the headless Service `rpc-server` for the ready workers, waits for
  `RPC_MIN_WORKERS` of them and the set to stay unchanged for 5 s, and runs
  `llama-server --rpc <ip:port,...> -ngl 999`: every layer goes to a
  worker, in proportion to the memory each reports, and the token
  embeddings stay on the server's node. The script keeps watching the
  answer: when the set changes it stops llama-server (SIGKILL after 10 s,
  since a worker whose node lost power never answers) and starts it again
  with the new set. llama.cpp's RPC client does not reconnect, and a new
  worker only gets layers when the model is loaded again.
- **`llama`**, a NodePort Service on port 8080: the OpenAI-compatible API
  (`/v1/chat/completions`, `/v1/completions`, `/v1/models`), `/health`
  and Prometheus metrics on `/metrics`. On a dev cluster
  the NodePort is also open on your machine's `localhost`.

The official llama.cpp container images are built without the RPC backend
(`GGML_RPC` is off in its Dockerfile); its release tarballs have it. So
both pods run the official `ghcr.io/ggml-org/llama.cpp:server-b11669` image
(llama.cpp v0.7.0), for its libraries, and an init container unpacks the
same build's tarball for the node's CPU, checked against a pinned sha256.
arm64 and amd64 nodes mix freely; nothing is built.

## Run it

On a dev cluster (macOS or Linux):

```bash
litek8s dev up --name llm --nodes 3
kubectl config use-context litek8s-llm
kubectl label node --all llm.litek8s.dev/rpc=true
kubectl apply -f examples/llm-rpc/
kubectl -n llm rollout status deploy/llama-server --timeout=10m

PORT=$(kubectl -n llm get svc llama -o jsonpath='{.spec.ports[0].nodePort}')
curl -s localhost:$PORT/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"In one sentence, what is a Kubernetes DaemonSet?"}],"max_tokens":60}'
```

All three nodes of a dev cluster share one VM or sandbox, so this shows the
mechanics, not the memory win. On real machines, join them first
([Multi-node](../README.md#multi-node-and-ha)), then label the ones that
should hold layers and set the model in
[`00-config.yaml`](../examples/llm-rpc/00-config.yaml):

```bash
kubectl label node desktop laptop nas llm.litek8s.dev/rpc=true
# 00-config.yaml: MODEL_URL, MODEL_FILE, MODEL_SHA256, RPC_MIN_WORKERS: "3"
kubectl apply -f examples/llm-rpc/
```

Any GGUF that llama.cpp loads works; `MODEL_SHA256` is the file's sha256
from its Hugging Face page (empty skips the check). To change the model
later, edit the ConfigMap and restart the server: `kubectl -n llm rollout
restart deploy/llama-server`. Remove everything with
`kubectl delete ns llm`.

### Tuning

- **Shares.** Each worker reports its node's total RAM (llama.cpp's CPU
  device cannot see a cgroup limit), and llama.cpp gives it layers in
  proportion. That is why the DaemonSet sets no memory limit: a limit
  below the share would get the worker OOM-killed instead of given fewer
  layers. To pin shares, add `--tensor-split 2,1,1` to `LLAMA_ARGS`, in
  the order of the `rpc workers:` log line (sorted by pod IP).
- **Context.** `LLAMA_ARGS: "-c 4096"`; the KV cache lands on the workers
  with their layers.
- **Seeing the split.** `-lv 4` in `LLAMA_ARGS` makes llama-server log the
  devices and how many MiB of the model each received (it also logs every
  request's details).
- **GPUs.** A node with a GPU runs a GPU build of `rpc-server` (llama.cpp's
  CUDA/Vulkan/ROCm tarballs and `server-cuda`/`server-vulkan` images); the
  worker then offers the GPU instead of the CPU. Not tested here.

## Measured

Same request every time (46 prompt tokens, 128 generated, temperature 0),
three runs after a warm-up, from [`bench.sh`](../examples/llm-rpc/bench.sh),
which reads llama-server's own timings. "Local" is `llama-bench` in the
server pod with no RPC at all. Generation is tokens/s of output; prompt is
tokens/s of prompt processing.

**Real multi-node cluster, amd64**: three Ubuntu 24.04 VMs (4 vCPU, 4 GiB
each) under qemu/KVM on a Ryzen 7 5800X, on a virtual LAN, joined with
`litek8s agent --join` (bench/cluster.sh), litek8s 0.1.2, crun 1.14.1.

| model | where | generation | prompt | model memory per worker |
|---|---|---:|---:|---|
| Qwen2.5 1.5B Q4_K_M (1.1 GB) | local, no RPC | 41.1 | 173 | – |
| | 1 worker (same node) | 33.0 | 92 | 935 MiB |
| | 3 workers on 3 nodes | 26.5 | 88 | 276 / 265 / 394 MiB |
| Qwen2.5 7B Q4_K_M (4.7 GB) | 3 workers on 3 nodes | 7.5 | 19.7 | 1336 / 1302 / 1530 MiB |

The 7B model's 4.4 GiB of weights do not fit in any one of these 3.9 GiB
nodes; spread over three, each node used 1.8–2.0 GiB of its memory with
everything else running (the workers' working sets were 1.5, 1.5 and
1.8 GB in `litek8s top`). The server node keeps the token embeddings (125
MiB for 1.5B, 292 MiB for 7B, memory-mapped from the model file). The
first start downloaded the 4.4 GiB file in 64 s, and loading it (the
layers sent to the workers) took about a minute; a later restart of
llama-server took 40 s, the workers reading their cached tensors from
disk.

**Dev cluster on a Mac** (Apple M3 Max, `litek8s dev up --name llm --nodes
3`, litek8s 0.1.2; the three nodes share one VM with 7 vCPUs):

| model | where | generation | prompt | model memory per worker |
|---|---|---:|---:|---|
| Qwen2.5 0.5B Q4_K_M (491 MB) | local, no RPC | 200 | 454 | – |
| | 1 worker | 104 | 228 | 374 MiB |
| | 3 workers | 75 | 220 | 90 / 79 / 205 MiB |

From `kubectl apply` on a fresh cluster to the first answer: 171 s (the
image on three nodes and the model download).

So the RPC hop alone costs a fifth (amd64) to a half (the fast Mac) of
the speed, and each extra machine a bit more. The last worker holds the
output layer and returns the logits for every token: the vocabulary size
times 4 bytes, about 600 KB per token for Qwen's 152k tokens (measured:
5.1 MB/s from that worker over the minute of the 1.5B benchmark), plus
the compute graph llama-server sends each step. Wired gigabit is fine; on
Wi-Fi the link sets the speed. Uneven shares, as above, are that output
layer.

## When a worker goes away

| what happened | the request in flight | the API again |
|---|---|---|
| a worker pod deleted (DaemonSet recreates it), Linux, 1.5B | fails at once: `curl: (52) Empty reply from server` | 12 s later, all 3 workers |
| the same, 7B | fails at once | 38 s later |
| the same, Mac dev cluster, 0.5B | fails at once | 18 s later |
| a node powered off (Linux, 1.5B, default 40 s node grace period) | hangs, then fails after 52 s | 62 s after the power-off, on the 2 workers left |
| that node powered on again | – | back on 3 workers 25 s after power-on, ~12 s of it the VM booting |

llama.cpp aborts llama-server when a worker's connection breaks
(`Remote RPC server crashed or returned malformed response`), which kills
the requests in flight; it has no retry or failover, so a client should
retry. The start script, not Kubernetes, restarts it, so the pod never
goes into CrashLoopBackOff. A node that loses power is noticed when its
heartbeats stop (`--node-monitor-grace-period`, 40 s): its pods turn
NotReady, drop out of the headless Service, and the script reloads the
model without it, provided the remaining workers have the memory for it.
Until then requests that need the dead worker wait on it.

## Watching it

- **Which workers, which share**: `kubectl -n llm logs deploy/llama-server
  | grep 'rpc workers'` lists the workers in use and every change; with
  `-lv 4`, the `load_tensors: RPC0[ip:port] model buffer size` lines give
  each worker's share.
- **CPU and memory per worker**: `litek8s top -n llm` (or `kubectl top
  pods -n llm`), and the Observe view of `litek8s ui` (9), which lists the
  pods by CPU or memory with 30-minute sparklines. A worker's memory is
  its share of the model. PromQL against the built-in metrics
  ([observability.md](observability.md)):

  ```bash
  litek8s query 'sum by (pod, node) (container_memory_working_set_bytes{namespace="llm"})'
  litek8s query 'sum by (pod) (rate(container_cpu_usage_seconds_total{namespace="llm"}[1m]))'
  litek8s query 'sum by (pod) (rate(container_network_transmit_bytes_total{namespace="llm"}[1m]))'
  ```

- **Tokens/s and queue**: llama-server's own `/metrics`
  (`llamacpp:predicted_tokens_seconds`, `llamacpp:prompt_tokens_seconds`,
  `llamacpp:requests_processing`, `llamacpp:requests_deferred`, ...). litek8s
  does not scrape application `/metrics` endpoints yet: read it with
  `curl localhost:$PORT/metrics`, or point a Prometheus at the `llama`
  Service.
- **Logs**: `litek8s logs` searches every pod's logs, the ones of
  restarted or deleted containers included, e.g. `litek8s logs -n llm
  -g 'RPC server crashed'`.

## Limits

- llama.cpp calls its RPC backend a proof of concept and insecure: keep
  workers on pod IPs (as here) and the cluster on a network you trust.
- No speed-up from more machines, as measured above; the point is fitting
  a model that does not fit.
- One llama-server; it is the single point of failure, and a worker that
  goes away costs every request in flight plus a model reload.
- Memory shares follow total RAM, not free RAM or limits; pin them with
  `--tensor-split` on machines that do other work.
