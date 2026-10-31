---
title: "One LLM across three 4 GiB machines: you get memory, not speed"
description: "Qwen2.5 7B needs 4.4 GiB of weights. No node had that. Split over three with llama.cpp's RPC backend on litek8s, it ran at 7.5 tokens/s."
date: 2026-10-30
tags: [llm, llama.cpp, home lab, multi-node]
docs: [llm-cluster, multi-node, observability]
translation: es/un-llm-entre-tres-maquinas
---

A model needs its weights in memory. Qwen2.5 7B, quantized to Q4_K_M, is
4.4 GiB of weights. We had three machines with 4 GiB each (3.9 GiB usable),
so it fit on none of them. Spread across all three, it answered at
**7.5 tokens per second**.

That is the whole post in one line, and the honest half matters as much as
the fun half: joining machines buys you memory, not speed. Every token
passes through every worker in turn and crosses the network. Three machines
are slower than one machine that could hold the model by itself. The point
is running a model that otherwise does not run at all.

## The pieces

[llama.cpp](https://github.com/ggml-org/llama.cpp) does the hard part. Its
RPC backend lets one `llama-server` keep model layers on other machines,
each running a small `rpc-server`. What it does not do is find those
machines, restart them, or notice when one disappears. That is the part
litek8s does, with plain Kubernetes objects:

```text
 OpenAI client ──▶ Service llama (NodePort) ──▶ llama-server (Deployment, model on a PVC)
                                                    │ --rpc ipA:50052,ipB:50052,ipC:50052
                     headless Service rpc-server ┄┄┄┤ (ready pod IPs, watched)
                                                    ├──▶ rpc-server on node A ─┐
                                                    ├──▶ rpc-server on node B  ├ DaemonSet,
                                                    └──▶ rpc-server on node C ─┘ layers in RAM
```

- **`rpc-server` is a DaemonSet** on every node labeled
  `llm.litek8s.dev/rpc=true`. It listens on its pod IP only, never on a
  host port. That matters: the RPC protocol has no authentication, and
  between nodes the traffic rides litek8s's WireGuard overlay. `--cache`
  keeps received tensors on the node's disk, so a restarted worker reads
  them locally instead of over the network.
- **`llama-server` is a Deployment of one.** Its start script asks the
  headless Service for the ready workers, waits until the set has stayed
  the same for 5 seconds, and runs `llama-server --rpc <workers> -ngl 999`.
  Every layer goes to a worker, in proportion to the memory each reports.
  When the set changes, the script restarts llama-server with the new one,
  because llama.cpp's RPC client does not reconnect.
- **`llama` is a NodePort Service** with the OpenAI-compatible API on port
  8080, `/health`, and Prometheus metrics on `/metrics`.

The official llama.cpp container images are built without the RPC backend;
its release tarballs have it. So the pods run the official image for its
libraries and an init container unpacks the same build's tarball, checked
against a pinned sha256. arm64 and amd64 nodes mix freely, and nothing is
compiled.

## The numbers

Same request every time (46 prompt tokens, 128 generated, temperature 0),
three runs after a warm-up, read from llama-server's own timings by
[`bench.sh`](https://github.com/jairoFernandez/litek8s/blob/main/examples/llm-rpc/bench.sh).
The cluster: three Ubuntu 24.04 VMs (4 vCPU, 4 GiB each) under qemu/KVM on
a Ryzen 7 5800X, on a virtual LAN, joined with `litek8s agent --join`.
"Local" is `llama-bench` in the server pod with no RPC at all.

| model | where | generation tok/s | prompt tok/s | model memory per worker |
|---|---|---:|---:|---|
| Qwen2.5 1.5B Q4_K_M (1.1 GB) | local, no RPC | 41.1 | 173 | – |
| | 1 worker (same node) | 33.0 | 92 | 935 MiB |
| | 3 workers on 3 nodes | 26.5 | 88 | 276 / 265 / 394 MiB |
| Qwen2.5 7B Q4_K_M (4.7 GB) | 3 workers on 3 nodes | 7.5 | 19.7 | 1336 / 1302 / 1530 MiB |

There is no "local" row for the 7B model, and that is the point: it does
not fit on any of these nodes. Spread over three, each node used 1.8 to
2.0 GiB of its memory with everything else running.

The 1.5B rows show the price. Going from no RPC (41.1 tokens/s) to one
worker on the same machine (33.0) to three workers on three machines
(26.5), each step costs speed. On a dev cluster on an Apple M3 Max, with a
0.5B model, the same three steps went 200 → 104 → 75. The RPC hop alone
costs a fifth (amd64) to a half (the fast Mac) of the speed, and each extra
machine a bit more.

Where does it go? The last worker holds the output layer and returns the
logits for every token: the vocabulary size times 4 bytes, about 600 KB
per token for Qwen's 152k-token vocabulary (measured: 5.1 MB/s from that
worker during the 1.5B benchmark), plus the compute graph llama-server
sends each step. Wired gigabit is fine. On Wi-Fi, the link sets the speed.
That output layer is also why the shares above are uneven.

Time to first answer on the 7B cluster: downloading the 4.4 GiB file took
64 s and loading it (sending layers to the workers) about a minute. A later
restart of llama-server took 40 s, with the workers reading their cached
tensors from disk.

## When a machine goes away

This is the part a homelab actually tests. We deleted worker pods and
powered off a node mid-request:

| what happened | the request in flight | API back |
|---|---|---|
| a worker pod deleted, 1.5B | fails at once | 12 s later, all 3 workers |
| the same, 7B | fails at once | 38 s later |
| a node powered off (1.5B, default 40 s node grace period) | hangs, then fails after 52 s | 62 s after the power-off, on the 2 workers left |
| that node powered on again | – | back on 3 workers 25 s after power-on |

llama.cpp has no retry or failover: when a worker's connection breaks it
aborts llama-server, and every request in flight dies with it. So your
client should retry. The start script, not Kubernetes, restarts the
server, so the pod never lands in CrashLoopBackOff. A powered-off node is
noticed when its heartbeats stop; its worker turns NotReady, drops out of
the headless Service, and the script reloads the model without it,
provided the remaining workers have the memory.

## Watching it

You want to know which machine holds what. Memory per worker is its share
of the model:

```bash
kubectl -n llm logs deploy/llama-server | grep 'rpc workers'   # which workers it uses
litek8s top -n llm                                            # memory per worker = its share
curl -s localhost:$PORT/metrics                               # llama-server's Prometheus metrics
```

The built-in metrics answer PromQL too, for example network out per pod
(see [Metrics, logs, alerts](/docs/observability/)):

```bash
litek8s query 'sum by (pod) (rate(container_network_transmit_bytes_total{namespace="llm"}[1m]))'
```

## What to keep in mind

- llama.cpp calls its RPC backend a proof of concept and insecure. Keep
  workers on pod IPs, as the manifests do, and the cluster on a network you
  trust.
- More machines will not make it faster. The measured numbers above say so.
- There is one llama-server. It is the single point of failure, and a
  worker that goes away costs every request in flight plus a model reload.
- Each worker reports its node's total RAM, not free RAM (llama.cpp cannot
  see a cgroup limit). On machines that do other work, pin the shares with
  `--tensor-split` in `LLAMA_ARGS`.
- GPU builds of `rpc-server` exist in llama.cpp; we have not tested them
  here.

## Try it

On a laptop, a 3-node dev cluster shows the mechanics (all three nodes
share one VM or sandbox, so not the memory win):

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

On a Mac, from `kubectl apply` on a fresh cluster to the first answer took
171 s, most of it pulling the image on three nodes and downloading the
model.

For the real thing, join your machines first ([Multi-node](/docs/multi-node/)),
then label the ones that should hold layers and set the model in
`00-config.yaml`:

```bash
kubectl label node desktop laptop nas llm.litek8s.dev/rpc=true
# 00-config.yaml: MODEL_URL, MODEL_FILE, MODEL_SHA256, RPC_MIN_WORKERS: "3"
kubectl apply -f examples/llm-rpc/
```

Any GGUF that llama.cpp loads works. The full guide, tuning and failure
details are in [One LLM across machines](/docs/llm-cluster/). Remove it
all with `kubectl delete ns llm`.
