# Distributed tensor compute — proof of concept

Run a matrix multiply (`A · B`) split across the real cluster nodes. This is the
building block of an LLM layer: the daemon shards `A` into **row bands**, ships
one band per schedulable node over the existing SSH-agent transport, each node
computes its band with its own cores, and the bands are stitched back into the
full product. No cross-node communication — embarrassingly parallel, exactly how
tensor/model parallelism shards a weight matmul across machines.

```
        ┌─────────── left (A) ───────────┐         right (B)
 node-0 │ rows   0..170                  │  ×  ┌──────────────┐
 node-1 │ rows 171..341                  │  ×  │  full matrix │  →  band → stitch
 local  │ rows 342..511                  │  ×  └──────────────┘
        └────────────────────────────────┘
```

## 1. Bring up the cluster (Terraform)

Two `t3.micro` EC2 workers (intentionally weak — the point is "máquinas no tan
potentes"):

```bash
cd infra/terraform
terraform init
terraform apply            # creates VPC, SG (SSH from your IP), 2 Ubuntu 24.04 nodes
terraform output node_add_commands
```

## 2. Start the daemon and register the nodes

```bash
litek8s serve &                                   # local control plane + Docker

# install the agent binary on each node (cross-compiles linux/amd64, scp's it)
litek8s node add litek8s-node-0 --ssh ubuntu@<ip0> --install --label role=worker
litek8s node add litek8s-node-1 --ssh ubuntu@<ip1> --install --label role=worker

litek8s get nodes                                 # all three should be Ready
```

> The agent must be the **current** binary — `tensor multiply` adds a new agent
> op (`ComputeMatmulBand`). If you registered nodes from an older build, re-run
> `node add --install` to refresh them.

## 3. Run the distributed multiply

Hand-verifiable 4×3 · 3×4 case:

```bash
litek8s tensor multiply -f examples/tensor-demo.yaml --show-result
```

Scale the work up to stress the weak nodes (deterministic, reproducible):

```bash
litek8s tensor multiply --size 256
litek8s tensor multiply --size 512 --seed 7
```

## What you see

```
Distributed Tensor Multiply
shapes 512x512 · 512x512 = 512x512

placement — row bands fanned across 3 node(s), wall 5539ms
  litek8s-node-0   Ready  rows     0..171   ( 171)  2727ms
  litek8s-node-1   Ready  rows   171..342   ( 171)  5539ms
  local            Schedulable rows   342..512   ( 170)  74ms

result rows 512
checksum (Σ) -212.519207
```

- **placement** — which node owned which rows, and how long its band took
  (including the SSH round-trip + serializing `B` for remote nodes).
- **checksum** — sum of all result cells; lets you confirm every node agreed on
  the math without dumping the full matrix. Same seed → same checksum.

## How it works (code map)

| Piece | File |
|-------|------|
| Band compute + deterministic input | [`matrix.rs`](../src/matrix.rs) — `multiply_band`, `deterministic_matrix` |
| Weight serialization + content hash | [`matrix.rs`](../src/matrix.rs) — `write_matrix_bin`, `read_matrix_bin`, `matrix_id` |
| Agent ops (compute + weight cache) | [`node.rs`](../src/node.rs) — `ComputeMatmulBand`, `PutWeights`, `HasWeights`, `ComputeMatmulBandCached` |
| Fan-out + stitch + cache orchestration | [`daemon.rs`](../src/daemon.rs) — `distributed_matmul` |
| CLI | [`cli.rs`](../src/cli.rs) — `tensor multiply` (`--no-cache`) |

## Weight caching (default on)

The right-hand matrix `B` is the "weights" of the op. Re-sending it on every band
call is the dominant cost on weak nodes over a WAN. So by default the daemon
**content-addresses `B`** (a hash of its contents), uploads it to each node's
on-disk cache (`~/.local/share/litek8s/weights/<id>.bin`) once, and the compute
call references the id — no per-band transfer. A repeat run with identical
weights skips the upload entirely.

```bash
litek8s tensor multiply --size 512 --seed 7              # run 1: [uploaded]
litek8s tensor multiply --size 512 --seed 7              # run 2: [cache hit]
litek8s tensor multiply --size 512 --seed 7 --no-cache   # re-send B every call
```

Measured on the two t3.micro EC2 nodes (512×512):

| Mode | Wall |
|------|------|
| `--no-cache` (re-send `B` each call) | ~8.1s |
| cache, first run (uploads `B`)       | ~7.4s |
| cache, warm (cache hit, no transfer) | ~2.2s |

~3.5× faster once the weights are resident — the cache survives agent restarts
because it's on disk. Each placement line is tagged `[uploaded]` / `[cache hit]`.

## Limits / next steps

- Caching covers the right-hand matrix. The next step toward serving a model is
  **chaining layers into a pipeline** — passing activations node→node instead of
  collecting every band back at the daemon between ops.
- Bands are split evenly by row count, not by node speed — a faster node finishes
  early and idles. A weighted split by `host_resources` CPU would balance better.
- Only `f64` dense matmul. Real model serving wants quantized/tiled kernels.
