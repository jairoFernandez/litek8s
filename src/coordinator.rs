//! In-cluster LLM coordinator: runs a Qwen2 forward from a node, sharding each
//! large weight matmul by columns across peer agents over the private LAN.
//!
//! The big projection weights (q/k/v/o, gate/up/down) live column-sharded in the
//! peers' RAM caches — no node holds the whole model. The coordinator keeps only
//! the small per-channel weights (RMSNorm scales, QKV biases) locally and does
//! the cheap, sequential parts (RoPE, softmax, residuals, SwiGLU activation)
//! itself, fanning each matmul to the peers and concatenating the column shards.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::llm::LlmConfig;
use crate::matrix::{self, BinDtype, Matrix};
use crate::node::{AgentRequest, AgentResponse, TcpAgentClient};
use crate::transformer::{add, grouped_query_attention, rms_norm, rope_tables, silu};

/// Where one weight tensor's column shards live: for each peer, the weight_id it
/// holds and the column range. Concatenating the peers' outputs in `col_start`
/// order reconstructs the full `activation · weight`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardedTensor {
    pub out_dim: usize,
    pub shards: Vec<TensorShard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TensorShard {
    pub peer_ip: String,
    pub weight_id: String,
    pub col_start: usize,
    pub cols: usize,
}

/// One transformer layer's distributed weights plus the small local ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistLayer {
    pub attn_norm: Vec<f64>,
    pub ffn_norm: Vec<f64>,
    pub bq: Vec<f64>,
    pub bk: Vec<f64>,
    pub bv: Vec<f64>,
    pub wq: ShardedTensor,
    pub wk: ShardedTensor,
    pub wv: ShardedTensor,
    pub wo: ShardedTensor,
    pub w_gate: ShardedTensor,
    pub w_up: ShardedTensor,
    pub w_down: ShardedTensor,
}

/// The embedding table (also the tied lm_head) kept on the coordinator at f16 —
/// row-major `vocab x hidden`, 2 bytes/elem so 151936x896 fits in ~0.27GB
/// instead of ~1.1GB at f64 (a t3.micro has ~0.9GB). Dequantized on the fly for
/// the embedding lookup and the final logits projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedTable {
    pub vocab: usize,
    pub hidden: usize,
    /// Row-major f16 little-endian bytes (vocab*hidden*2).
    pub f16_bytes: Vec<u8>,
}

impl EmbedTable {
    /// Dequantize row `idx` (a token's embedding) to f64.
    fn row(&self, idx: usize) -> Vec<f64> {
        let off = idx * self.hidden * 2;
        (0..self.hidden)
            .map(|j| {
                let b = off + j * 2;
                f64::from(half::f16::from_le_bytes([self.f16_bytes[b], self.f16_bytes[b + 1]]).to_f32())
            })
            .collect()
    }

    /// Tied lm_head: logits[t] = embed_row(t) · last_hidden, over the whole
    /// vocab. The dominant cost per token (151936 dots). Cast the hidden vector
    /// to f32 once, then dot each embedding row in f32 directly from the f16
    /// bytes — no per-row Vec allocation, autovectorizable inner loop. Parallel
    /// over the vocab.
    fn logits(&self, last: &[f64]) -> Vec<f64> {
        let last_f32: Vec<f32> = last.iter().map(|&x| x as f32).collect();
        let hidden = self.hidden;
        let vocab = self.vocab;
        let bytes = &self.f16_bytes;

        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(vocab.max(1));
        let chunk = vocab.div_ceil(workers);

        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for start in (0..vocab).step_by(chunk) {
                let end = (start + chunk).min(vocab);
                let last_f32 = &last_f32;
                handles.push(scope.spawn(move || {
                    (start..end)
                        .map(|t| {
                            let off = t * hidden * 2;
                            let mut acc = 0.0f32;
                            for j in 0..hidden {
                                let b = off + j * 2;
                                let w = half::f16::from_le_bytes([bytes[b], bytes[b + 1]]).to_f32();
                                acc += w * last_f32[j];
                            }
                            acc as f64
                        })
                        .collect::<Vec<f64>>()
                }));
            }
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        })
    }
}

/// A Qwen2 model distributed across peers: small weights local, big ones
/// sharded. Holds the f16 embedding table (also the tied lm_head) locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistModel {
    pub cfg: LlmConfigDump,
    pub embed: EmbedTable,
    pub final_norm: Vec<f64>,
    pub layers: Vec<DistLayer>,
}

/// The persistable part of a distributed model: everything except the big
/// embedding table. Small (KB-MB: config, norm/bias vectors, shard manifests),
/// so writing/reading it is instant — the embedding is re-read from the model
/// file's mmap at generate time (one tensor, not all 290). This lets `preload`
/// pay the ~28s distribution once and later `generate` runs start cold in ~1s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistManifest {
    pub cfg: LlmConfigDump,
    pub final_norm: Vec<f64>,
    pub layers: Vec<DistLayer>,
}

impl DistModel {
    /// Split into a persistable manifest (drops the embedding) — paired with
    /// `from_manifest` which re-attaches the embedding from the model file.
    pub fn into_manifest(self) -> DistManifest {
        DistManifest {
            cfg: self.cfg,
            final_norm: self.final_norm,
            layers: self.layers,
        }
    }

    /// Rebuild a runnable model from a manifest plus the embedding read fresh
    /// from the model file (mmap, one tensor).
    pub fn from_manifest(manifest: DistManifest, model_path: &std::path::Path) -> Result<Self> {
        let file = crate::model::ModelFile::open(model_path)?;
        let (vocab, hidden, f16_bytes) = file.tensor_2d_f16("model.embed_tokens.weight")?;
        Ok(Self {
            cfg: manifest.cfg,
            embed: EmbedTable {
                vocab,
                hidden,
                f16_bytes,
            },
            final_norm: manifest.final_norm,
            layers: manifest.layers,
        })
    }
}

/// Plain-data copy of the bits of LlmConfig the coordinator forward needs
/// (LlmConfig isn't Serialize; this is, so the whole DistModel can be persisted
/// or shipped).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfigDump {
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub num_key_value_heads: usize,
    pub vocab_size: usize,
    pub rms_norm_eps: f64,
    pub rope_theta: f64,
}

impl LlmConfigDump {
    fn head_dim(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

/// Multiply `activation` against a column-sharded weight by fanning each shard to
/// its peer over TCP-LAN and concatenating the results in column order.
async fn sharded_matmul(activation: &Matrix, tensor: &ShardedTensor) -> Result<Matrix> {
    let mut tasks = Vec::new();
    for shard in &tensor.shards {
        let client = TcpAgentClient::new(&shard.peer_ip);
        let weight_id = shard.weight_id.clone();
        let act = activation.clone();
        let col_start = shard.col_start;
        tasks.push(tokio::spawn(async move {
            let resp = client
                .request(AgentRequest::ComputeLayerShardCached {
                    activation: act,
                    weight_id,
                    relu: false,
                })
                .await;
            (col_start, resp)
        }));
    }

    let mut parts: Vec<(usize, Matrix)> = Vec::new();
    for task in tasks {
        let (col_start, resp) = task.await.context("shard matmul task panicked")?;
        match resp? {
            AgentResponse::MatmulBand { rows } => parts.push((col_start, rows)),
            other => anyhow::bail!("peer returned unexpected response: {other:?}"),
        }
    }
    parts.sort_by_key(|(s, _)| *s);

    let rows = activation.len();
    let mut out = vec![Vec::with_capacity(tensor.out_dim); rows];
    for (_, part) in parts {
        for (i, row) in part.into_iter().enumerate() {
            out[i].extend(row);
        }
    }
    Ok(out)
}

/// Add a bias vector to every row (in place), no-op if empty.
fn add_bias(m: &mut Matrix, bias: &[f64]) {
    if bias.is_empty() {
        return;
    }
    for row in m {
        for (v, b) in row.iter_mut().zip(bias.iter()) {
            *v += b;
        }
    }
}

impl DistModel {
    /// One incremental decode step with a per-layer KV cache: process a SINGLE
    /// token (its embedding row), reusing cached keys/values for all prior
    /// positions. Every sharded matmul runs on one row instead of the whole
    /// prefix, so the per-token cost stops growing with sequence length. Returns
    /// the vocab logits for this position. `pos` is the 0-based position of this
    /// token in the sequence; `caches` has one KvCache per layer.
    pub async fn decode_step(
        &self,
        token: u32,
        pos: usize,
        caches: &mut [crate::transformer::KvCache],
    ) -> Result<Vec<f64>> {
        let cfg = &self.cfg;
        let head_dim = cfg.head_dim();
        // RoPE angles for this single position.
        let (cos_t, sin_t) = {
            let (c, s) = rope_tables(pos + 1, head_dim, cfg.rope_theta);
            (c[pos].clone(), s[pos].clone())
        };

        // Single-row hidden state.
        let mut h: Matrix = vec![self.embed.row(token as usize)];

        for (li, layer) in self.layers.iter().enumerate() {
            let normed = rms_norm(&h, &layer.attn_norm, cfg.rms_norm_eps);
            let mut q = sharded_matmul(&normed, &layer.wq).await?;
            let mut k = sharded_matmul(&normed, &layer.wk).await?;
            let mut v = sharded_matmul(&normed, &layer.wv).await?;
            add_bias(&mut q, &layer.bq);
            add_bias(&mut k, &layer.bk);
            add_bias(&mut v, &layer.bv);

            let context = crate::transformer::gqa_step(
                &q[0],
                &k[0],
                &v[0],
                &mut caches[li],
                cfg.num_attention_heads,
                cfg.num_key_value_heads,
                head_dim,
                pos,
                &cos_t,
                &sin_t,
            );
            let attn_out = sharded_matmul(&vec![context], &layer.wo).await?;
            h = add(&h, &attn_out);

            let ffn_in = rms_norm(&h, &layer.ffn_norm, cfg.rms_norm_eps);
            let gate = sharded_matmul(&ffn_in, &layer.w_gate).await?;
            let up = sharded_matmul(&ffn_in, &layer.w_up).await?;
            let hidden: Matrix = gate
                .iter()
                .zip(up.iter())
                .map(|(g, u)| {
                    g.iter()
                        .zip(u.iter())
                        .map(|(gv, uv)| silu(*gv) * uv)
                        .collect()
                })
                .collect();
            let ffn_out = sharded_matmul(&hidden, &layer.w_down).await?;
            h = add(&h, &ffn_out);
        }

        let normed = rms_norm(&h, &self.final_norm, cfg.rms_norm_eps);
        Ok(self.embed.logits(&normed[0]))
    }

    /// Fresh per-layer KV caches for a new generation.
    pub fn new_caches(&self) -> Vec<crate::transformer::KvCache> {
        vec![crate::transformer::KvCache::default(); self.layers.len()]
    }

    /// Run the distributed forward for a token sequence and return the last
    /// position's vocab logits. Same math as the local `forward_last_logits`,
    /// but every big matmul is sharded across peers over the LAN.
    pub async fn forward_last_logits(&self, tokens: &[u32]) -> Result<Vec<f64>> {
        let cfg = &self.cfg;
        let head_dim = cfg.head_dim();
        let seq = tokens.len();

        let mut h: Matrix = tokens.iter().map(|&t| self.embed.row(t as usize)).collect();
        let (cos, sin) = rope_tables(seq, head_dim, cfg.rope_theta);

        for layer in &self.layers {
            let normed = rms_norm(&h, &layer.attn_norm, cfg.rms_norm_eps);
            let mut q = sharded_matmul(&normed, &layer.wq).await?;
            let mut k = sharded_matmul(&normed, &layer.wk).await?;
            let mut v = sharded_matmul(&normed, &layer.wv).await?;
            add_bias(&mut q, &layer.bq);
            add_bias(&mut k, &layer.bk);
            add_bias(&mut v, &layer.bv);

            let context = grouped_query_attention(
                &q,
                &k,
                &v,
                cfg.num_attention_heads,
                cfg.num_key_value_heads,
                head_dim,
                &cos,
                &sin,
            );
            let attn_out = sharded_matmul(&context, &layer.wo).await?;
            h = add(&h, &attn_out);

            let ffn_in = rms_norm(&h, &layer.ffn_norm, cfg.rms_norm_eps);
            let gate = sharded_matmul(&ffn_in, &layer.w_gate).await?;
            let up = sharded_matmul(&ffn_in, &layer.w_up).await?;
            let hidden: Matrix = gate
                .iter()
                .zip(up.iter())
                .map(|(g, u)| {
                    g.iter()
                        .zip(u.iter())
                        .map(|(gv, uv)| silu(*gv) * uv)
                        .collect()
                })
                .collect();
            let ffn_out = sharded_matmul(&hidden, &layer.w_down).await?;
            h = add(&h, &ffn_out);
        }

        let normed = rms_norm(&h, &self.final_norm, cfg.rms_norm_eps);
        // Tied lm_head over the whole vocab (the dominant per-token cost).
        Ok(self.embed.logits(&normed[seq - 1]))
    }
}

/// Greedy argmax over logits.
pub fn argmax(logits: &[f64]) -> usize {
    logits
        .iter()
        .enumerate()
        .fold((0, f64::NEG_INFINITY), |(bi, bv), (i, &v)| {
            if v > bv { (i, v) } else { (bi, bv) }
        })
        .0
}

/// Transpose an `out x in` HF weight into `in x out` (so `activation · w` works).
fn transpose(m: &[Vec<f64>]) -> Matrix {
    if m.is_empty() {
        return Vec::new();
    }
    let (r, c) = (m.len(), m[0].len());
    (0..c).map(|j| (0..r).map(|i| m[i][j]).collect()).collect()
}

/// Split a weight (already `in x out`) into column shards across `peers`,
/// weighted by `peer_weights` (e.g. each peer's RAM in GB) so a weak node gets
/// fewer columns and doesn't run out of memory. Uploads each shard column-major
/// to its peer's cache over TCP. `dtype` keeps native precision on the wire/disk.
async fn distribute_tensor(
    weight_in_out: &Matrix,
    peers: &[String],
    peer_weights: &[u32],
    dtype: BinDtype,
    tag: &str,
) -> Result<ShardedTensor> {
    let out_dim = weight_in_out.first().map(|r| r.len()).unwrap_or(0);
    let cols = matrix::columns(weight_in_out); // column-major: cols[j] = column j
    // Columns per peer proportional to capacity (RAM), exact sum.
    let splits = matrix::weighted_split(out_dim, peer_weights);

    let mut tasks = Vec::new();
    let mut cursor = 0;
    for (peer, here) in peers.iter().zip(splits.iter().copied()) {
        let start = cursor;
        cursor += here;
        if here == 0 {
            continue;
        }
        let shard: Matrix = cols[start..start + here].to_vec();
        let weight_id = format!("{tag}-{}", matrix::matrix_id(&shard));
        let blob = matrix::encode_matrix_bin_dtype(&shard, dtype)?;
        let client = TcpAgentClient::new(peer);
        let peer_ip = peer.clone();
        tasks.push(tokio::spawn(async move {
            let resp = client
                .request_blob(
                    AgentRequest::PutWeights {
                        weight_id: weight_id.clone(),
                        byte_len: blob.len() as u64,
                    },
                    &blob,
                )
                .await;
            (peer_ip, weight_id, start, here, resp)
        }));
    }

    let mut shards = Vec::new();
    for task in tasks {
        let (peer_ip, weight_id, col_start, cols_here, resp) =
            task.await.context("distribute task panicked")?;
        match resp? {
            AgentResponse::Ack => shards.push(TensorShard {
                peer_ip,
                weight_id,
                col_start,
                cols: cols_here,
            }),
            other => anyhow::bail!("peer {peer_ip} rejected weight upload: {other:?}"),
        }
    }
    shards.sort_by_key(|s| s.col_start);
    Ok(ShardedTensor { out_dim, shards })
}

/// Build a DistModel from a loaded Qwen checkpoint: small weights kept local,
/// each big projection transposed, sharded by columns, and uploaded to `peers`.
pub async fn distribute_qwen(
    model_path: &std::path::Path,
    cfg: &LlmConfig,
    peers: &[String],
    dtype: BinDtype,
) -> Result<DistModel> {
    // Weight each peer by its RAM (GB), so a weak node gets fewer columns and
    // doesn't OOM on a big model's shard. Query HostResources once; default to 1
    // if a peer doesn't answer.
    let mut peer_weights = Vec::with_capacity(peers.len());
    for ip in peers {
        let client = TcpAgentClient::new(ip);
        let w = match client.request(AgentRequest::HostResources).await {
            Ok(AgentResponse::HostResources { resources }) => {
                ((resources.memory_total_bytes / 1_000_000_000).max(1)) as u32
            }
            _ => 1,
        };
        peer_weights.push(w);
    }

    // mmap the file once — the OS pages tensors in on demand, so the whole
    // multi-GB model never lands in the coordinator's RAM at once.
    let file = crate::model::ModelFile::open(model_path)?;

    let (vocab, hidden, f16_bytes) = file.tensor_2d_f16("model.embed_tokens.weight")?;
    let embed = EmbedTable {
        vocab,
        hidden,
        f16_bytes,
    };
    let final_norm = file.tensor_1d("model.norm.weight");

    let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
    for i in 0..cfg.num_hidden_layers {
        let p = format!("model.layers.{i}");
        // Load + transpose + distribute one tensor; each is freed before the
        // next so peak RAM stays at one layer-weight, not the whole model.
        let mut big = |name: String, tag: String| -> Result<(Matrix, String)> {
            Ok((transpose(&file.tensor_2d(&name)?), tag))
        };

        let attn_norm = file.tensor_1d(&format!("{p}.input_layernorm.weight"));
        let ffn_norm = file.tensor_1d(&format!("{p}.post_attention_layernorm.weight"));
        let bq = file.tensor_1d(&format!("{p}.self_attn.q_proj.bias"));
        let bk = file.tensor_1d(&format!("{p}.self_attn.k_proj.bias"));
        let bv = file.tensor_1d(&format!("{p}.self_attn.v_proj.bias"));

        let (wq_m, _) = big(format!("{p}.self_attn.q_proj.weight"), format!("l{i}-q"))?;
        let wq = distribute_tensor(&wq_m, peers, &peer_weights, dtype, &format!("l{i}-q")).await?;
        drop(wq_m);
        let (wk_m, _) = big(format!("{p}.self_attn.k_proj.weight"), format!("l{i}-k"))?;
        let wk = distribute_tensor(&wk_m, peers, &peer_weights, dtype, &format!("l{i}-k")).await?;
        drop(wk_m);
        let (wv_m, _) = big(format!("{p}.self_attn.v_proj.weight"), format!("l{i}-v"))?;
        let wv = distribute_tensor(&wv_m, peers, &peer_weights, dtype, &format!("l{i}-v")).await?;
        drop(wv_m);
        let (wo_m, _) = big(format!("{p}.self_attn.o_proj.weight"), format!("l{i}-o"))?;
        let wo = distribute_tensor(&wo_m, peers, &peer_weights, dtype, &format!("l{i}-o")).await?;
        drop(wo_m);
        let (wg_m, _) = big(format!("{p}.mlp.gate_proj.weight"), format!("l{i}-g"))?;
        let w_gate = distribute_tensor(&wg_m, peers, &peer_weights, dtype, &format!("l{i}-g")).await?;
        drop(wg_m);
        let (wu_m, _) = big(format!("{p}.mlp.up_proj.weight"), format!("l{i}-u"))?;
        let w_up = distribute_tensor(&wu_m, peers, &peer_weights, dtype, &format!("l{i}-u")).await?;
        drop(wu_m);
        let (wd_m, _) = big(format!("{p}.mlp.down_proj.weight"), format!("l{i}-d"))?;
        let w_down = distribute_tensor(&wd_m, peers, &peer_weights, dtype, &format!("l{i}-d")).await?;
        drop(wd_m);

        layers.push(DistLayer {
            attn_norm,
            ffn_norm,
            bq,
            bk,
            bv,
            wq,
            wk,
            wv,
            wo,
            w_gate,
            w_up,
            w_down,
        });
    }

    Ok(DistModel {
        cfg: LlmConfigDump {
            hidden_size: cfg.hidden_size,
            num_hidden_layers: cfg.num_hidden_layers,
            num_attention_heads: cfg.num_attention_heads,
            num_key_value_heads: cfg.num_key_value_heads,
            vocab_size: cfg.vocab_size,
            rms_norm_eps: cfg.rms_norm_eps,
            rope_theta: cfg.rope_theta,
        },
        embed,
        final_norm,
        layers,
    })
}
