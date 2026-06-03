# Distributed model — running real weights across the cluster

Load a real `safetensors` model, shard every layer's weight by **columns**
across the nodes (weighted by CPU), and run inference against the **resident**
weights. The full model never lives in one place — the daemon reads one layer at
a time while distributing, each node keeps only its column shards on disk, and a
manifest records who holds what. This is the payoff of the vision: a model too
large for any one box, split across the household.

## Prepare a model

Any `.safetensors` file with 2D weight tensors works (Llama/Mistral layers are
exactly this). A tiny chained net for the demo:

```python
import numpy as np
from safetensors.numpy import save_file
rng = np.random.default_rng(0)
save_file({
    "blk.0.w": (rng.standard_normal((512, 512)) * 0.05).astype(np.float32),
    "blk.1.w": (rng.standard_normal((512, 512)) * 0.05).astype(np.float32),
    "blk.2.w": (rng.standard_normal((512, 256)) * 0.05).astype(np.float32),
}, "net.safetensors")
```

## Distribute and run

```bash
litek8s serve &

litek8s model inspect net.safetensors          # list tensors, dtypes, shapes

# shard each layer's columns across the nodes, upload shards, persist manifest
litek8s model load mynet -f net.safetensors \
    --tensors blk.0.w --tensors blk.1.w --tensors blk.2.w

litek8s model list                              # which nodes hold the model
litek8s model run mynet --batch 1 --seed 5      # inference on resident weights
litek8s model rm mynet                          # forget the manifest
```

`--tensors` sets the layer order; omit it to take every 2D tensor sorted by
name. Supported weight dtypes: F64, F32, F16, BF16 (quantized formats need
dequantization, a later step).

## What you see

`model load` — each layer's columns split by CPU, uploaded once per node:

```
layer 0 blk.0.w — 512x512
    litek8s-node-0    2c  cols      0..39     (   39)   156 KB   3341ms  [uploaded]
    ...
    local            14c  cols    234..512    (  278)  1112 KB      0ms  [uploaded]
```

`model run` — activation flows through the resident shards (no upload):

```
layer 0 — 512x512 +relu took 281ms
layer 2 — 512x256 +relu took 343ms
output 1x256
checksum (Σ) 23.630838
```

## Correctness

The distributed inference equals a plain local forward. Verified two ways:

- `tests/model_load.rs` — safetensors decode (F32/F16/BF16) and column-shard
  extraction are byte-exact.
- Cross-checked against numpy: same input, same ReLU stack, identical checksum
  (`23.630838`).

## How it works

| Piece | File |
|-------|------|
| safetensors read + column-shard extract | [`model.rs`](../src/model.rs) — `inspect`, `load_tensor_2d`, `load_layer_column_shard` |
| Model manifest (who holds what) | [`model.rs`](../src/model.rs) — `ModelManifest` / `LayerManifest` / `ShardManifest`, persisted in [`store.rs`](../src/store.rs) |
| Distribute + infer | [`daemon.rs`](../src/daemon.rs) — `load_model`, `run_model` |
| CLI | [`cli.rs`](../src/cli.rs) — `model inspect / load / list / run / rm` |

## Wire format

Weight uploads ship as a **raw binary blob** after the request line
(`<json>\n<blob>`), not as JSON float text — 8 bytes/f64 instead of ~18 chars.
On the 88 MB model across 7 nodes this cut cold distribution from ~250 s (JSON)
to ~98 s, and removed the `Broken pipe` failures that large JSON payloads caused
on weak nodes. Inference is unchanged (resident weights).

## Limits / next steps

- The daemon reads one **layer** fully while sharding it — fine for layers that
  fit transiently, but a single huge layer (a 70B model's 8192² weight ≈ 500 MB
  in f32) still needs streamed/partial reads. safetensors supports it; not wired
  yet.
- Only matmul + ReLU. A real transformer needs attention, norms, embeddings — a
  richer layer graph than a linear stack.
- Dense f64 on the wire and in compute. Quantized weights (GGUF/int8) would suit
  weak home nodes far better.
- If a node holding a shard goes offline, `model run` errors clearly rather than
  re-sharding. Replication / re-distribution is future work.
