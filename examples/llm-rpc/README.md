# One LLM across all your machines (llama.cpp RPC)

Runs one model split across every node you opt in, with
[llama.cpp](https://github.com/ggml-org/llama.cpp)'s RPC backend: an
`rpc-server` per node (a DaemonSet) holds a share of the layers in its
memory, and one `llama-server` (a Deployment) loads the model, spreads it
over them and serves the OpenAI-compatible API. The win is memory: a model
too big for any one machine runs on several. It is not faster than one
machine that can hold the model. Full guide, numbers and failure behavior:
[docs/llm-cluster.md](../../docs/llm-cluster.md).

| file | what |
|---|---|
| [`00-config.yaml`](00-config.yaml) | namespace `llm`, the model (URL, file, sha256), llama.cpp build, `LLAMA_ARGS`, the pods' scripts |
| [`rpc-server.yaml`](rpc-server.yaml) | DaemonSet on nodes labeled `llm.litek8s.dev/rpc=true`, headless Service `rpc-server` |
| [`llama-server.yaml`](llama-server.yaml) | model cache PVC, Deployment, NodePort Service `llama` (port 8080) |
| [`bench.sh`](bench.sh) | prompt and generation tokens/s from the server's own timings |

The official llama.cpp images are built without the RPC backend, so the
pods run the official `ghcr.io/ggml-org/llama.cpp:server-b11669` image (it
has the libraries) and an init container unpacks the same build's release
tarball (RPC built in, sha256-checked) for the node's CPU: arm64 and amd64
work without building anything.

## On a dev cluster

```bash
litek8s dev up --name llm --nodes 3
kubectl config use-context litek8s-llm
kubectl label node --all llm.litek8s.dev/rpc=true
kubectl apply -f examples/llm-rpc/
kubectl -n llm rollout status deploy/llama-server --timeout=10m

PORT=$(kubectl -n llm get svc llama -o jsonpath='{.spec.ports[0].nodePort}')
curl -s localhost:$PORT/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"What is a DaemonSet?"}],"max_tokens":60}'
examples/llm-rpc/bench.sh http://localhost:$PORT
```

The first start pulls the image (~300 MB) on every node and downloads the
model (Qwen2.5 0.5B, 491 MB): 171 s on a 3-node dev cluster on a Mac.

## On real machines

Same manifests on a cluster of `litek8s apiserver` + `litek8s agent --join`
nodes. Label the machines that should hold layers, pick a bigger model in
`00-config.yaml` (and `RPC_MIN_WORKERS` = the number of workers, so the
model is not loaded before they are all up), then apply:

```bash
kubectl label node desktop laptop nas llm.litek8s.dev/rpc=true
kubectl apply -f examples/llm-rpc/
curl -s http://<any node>:<nodePort>/v1/models
```

## Watching it

```bash
kubectl -n llm logs deploy/llama-server | grep 'rpc workers'   # which workers it uses
litek8s top -n llm                                            # memory per worker = its share
curl -s localhost:$PORT/metrics                               # llama-server's Prometheus metrics
```

Add `-lv 4` to `LLAMA_ARGS` to log how many MiB of the model went to each
worker. Remove it all with `kubectl delete ns llm` (the downloaded model
goes with its volume).
