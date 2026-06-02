# Distributed layer pipeline — proof of concept

Run a feed-forward network across the cluster. The input activation flows
through each layer's weight matmul in order; **within** a layer the weight is
sharded by **columns** across the nodes, weighted by each node's CPU cores. Every
node holds only its slice of the weight — so a model too large for any single
node is split across many. ReLU runs between layers so it behaves like a real
network, not a single collapsed linear map.

```
            input (activation, replicated to all nodes)
                            │
   layer 0   ┌──────────────┴──────────────┐   W0 columns sharded by CPU
   local ←── cols 0..556          node-0 ←─ cols 556..634   node-1 ←─ ...
             └──────────────┬──────────────┘
                  concat columns = activation
                            │
   layer 1            (same shard, next weight)  ← activation flows down
                            │
                          output
```

This is the difference from `tensor multiply`: that splits **one** matmul by
rows for speed. This splits the **model** by columns across **layers** for
*capacity* — the reason to use weak nodes is aggregate memory, not FLOPs.

## Run it

```bash
litek8s serve &
litek8s get nodes                                         # nodes Ready

# hand-checkable tiny model
litek8s pipeline run -f examples/pipeline-demo.yaml --show-output

# generate a large deterministic model that fills the nodes
litek8s pipeline run --layers 4 --dim 1024                # 4 layers of 1024x1024
litek8s pipeline run --layers 6 --dim 512 --batch 2
```

`--layers N --dim D` builds N square `D×D` layers (distinct seed each, ReLU
between). The model is generated from seeds on each node — only the small spec
crosses the wire, never the weights.

## What you see

```
layer 0 — 1024x1024 +relu took 5093ms
    litek8s-node-0    2c  cols     0..78    (  78)    5004ms  [uploaded]
    ...
    local            14c  cols   468..1024  ( 556)       6ms  [uploaded]
```

- **cols a..b** — the column slice of this layer's weight that the node owns.
- **weighted split** — local (14 cores) gets 556 columns; each EC2 (2 cores) gets
  78. Proportional to CPU, so a fast node carries more.
- **[uploaded] / [cache hit]** — each shard is cached per node by content hash.
  First run uploads; once the model is resident, repeat runs are cache hits and
  skip all weight transfer.
- **checksum** — sum of the output; identical across runs for the same seed,
  and matches a plain local forward (verified in `tests/matrix_parallel.rs`:
  `sharded_pipeline_matches_local_forward`).

Measured (4 layers × 1024², 7 nodes): ~18s cold (uploads 32 MB of weights) vs
~6.4s warm (model resident, cache hits).

## Honest limits

- **Stragglers dominate.** Each layer's wall is its slowest shard. With hardware
  this uneven (Mac 14c vs EC2 2c) the weak EC2 nodes set the pace even though
  they hold few columns. Weighted split balances *load*, not the burstable-CPU
  variance of t3.micro.
- **The value here is memory, not speed.** On comparable hardware the speedup
  would be real; on a strong-Mac + weak-cloud mix, the payoff is fitting a model
  that wouldn't fit on one node — aggregate RAM across the household.
- **Dense f64, generated weights.** A real model would load trained weights from
  disk (already supported via the weight cache) and use quantized/tiled kernels.

## Code map

| Piece | File |
|-------|------|
| Column shard + weighted split + ReLU | [`matrix.rs`](../src/matrix.rs) — `multiply_column_shard`, `columns`, `weighted_split`, `apply_relu` |
| Agent op (per-node shard compute) | [`node.rs`](../src/node.rs) — `ComputeLayerShardCached` |
| Pipeline orchestration | [`daemon.rs`](../src/daemon.rs) — `distributed_pipeline` |
| CLI | [`cli.rs`](../src/cli.rs) — `pipeline run` |
