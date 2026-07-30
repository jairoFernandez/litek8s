//! Transformer building blocks — the math that turns the distributed matmul
//! into a real language model. Llama/DeepSeek-style: RMSNorm, rotary position
//! embeddings (RoPE), multi-head causal self-attention, and a SwiGLU
//! feed-forward network, with residual connections.
//!
//! Everything here operates on row-major `Vec<Vec<f64>>` activations (one row
//! per token) and is deterministic, so it can be validated against a reference
//! implementation (numpy/HF) before any of it is distributed across nodes.

use crate::matrix::Matrix;

/// Root-mean-square layer norm (no mean subtraction, no bias): for each token
/// row `x`, `out = x / sqrt(mean(x^2) + eps) * weight`. This is the norm
/// Llama/DeepSeek use instead of LayerNorm.
pub fn rms_norm(x: &[Vec<f64>], weight: &[f64], eps: f64) -> Matrix {
    x.iter()
        .map(|row| {
            debug_assert_eq!(row.len(), weight.len());
            let mean_sq = row.iter().map(|v| v * v).sum::<f64>() / row.len() as f64;
            let inv = 1.0 / (mean_sq + eps).sqrt();
            row.iter()
                .zip(weight.iter())
                .map(|(v, w)| v * inv * w)
                .collect()
        })
        .collect()
}

/// Numerically stable softmax over a slice (subtract max before exp).
pub fn softmax(scores: &[f64]) -> Vec<f64> {
    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = scores.iter().map(|s| (s - max).exp()).collect();
    let sum: f64 = exps.iter().sum();
    if sum == 0.0 {
        return vec![0.0; scores.len()];
    }
    exps.iter().map(|e| e / sum).collect()
}

/// SiLU / swish activation: `x * sigmoid(x)`.
pub fn silu(x: f64) -> f64 {
    x / (1.0 + (-x).exp())
}

/// Precomputed rotary (RoPE) cos/sin tables for positions `0..seq_len` over a
/// head dimension `head_dim` (must be even). `theta` is the base (10000 for
/// Llama). Returns (cos, sin), each `seq_len x head_dim/2`.
pub fn rope_tables(seq_len: usize, head_dim: usize, theta: f64) -> (Matrix, Matrix) {
    let mut cos = Vec::with_capacity(seq_len);
    let mut sin = Vec::with_capacity(seq_len);
    for pos in 0..seq_len {
        let (c, s) = rope_at(pos, head_dim, theta);
        cos.push(c);
        sin.push(s);
    }
    (cos, sin)
}

/// RoPE cos/sin values for one position without allocating tables for the
/// entire prefix. Incremental decoding calls this once per new token.
pub fn rope_at(pos: usize, head_dim: usize, theta: f64) -> (Vec<f64>, Vec<f64>) {
    let half = head_dim / 2;
    let mut cos = Vec::with_capacity(half);
    let mut sin = Vec::with_capacity(half);
    for index in 0..half {
        let frequency = 1.0 / theta.powf((2 * index) as f64 / head_dim as f64);
        let angle = pos as f64 * frequency;
        cos.push(angle.cos());
        sin.push(angle.sin());
    }
    (cos, sin)
}

/// Apply RoPE in place to one head's vector at a given position, using the
/// "rotate halves" convention (Llama/HF): the first and second halves of the
/// `head_dim` vector are rotated against each other.
pub fn apply_rope(vec: &mut [f64], cos: &[f64], sin: &[f64]) {
    let half = vec.len() / 2;
    for i in 0..half {
        let (a, b) = (vec[i], vec[i + half]);
        vec[i] = a * cos[i] - b * sin[i];
        vec[i + half] = b * cos[i] + a * sin[i];
    }
}

/// Grouped-query causal self-attention (batch = 1). `q` is
/// `seq x (n_q_heads*head_dim)`, `k`/`v` are `seq x (n_kv_heads*head_dim)` with
/// `n_q_heads` a multiple of `n_kv_heads` — each KV head is shared by
/// `n_q_heads/n_kv_heads` query heads (GQA, as in Qwen/Llama/DeepSeek). RoPE is
/// applied per head/position. Returns `seq x (n_q_heads*head_dim)`.
pub fn grouped_query_attention(
    q: &[Vec<f64>],
    k: &[Vec<f64>],
    v: &[Vec<f64>],
    n_q_heads: usize,
    n_kv_heads: usize,
    head_dim: usize,
    cos: &[Vec<f64>],
    sin: &[Vec<f64>],
) -> Matrix {
    let seq = q.len();
    let scale = 1.0 / (head_dim as f64).sqrt();
    let group = n_q_heads / n_kv_heads;
    let mut out = vec![vec![0.0; n_q_heads * head_dim]; seq];

    for qh_idx in 0..n_q_heads {
        let kv = qh_idx / group; // which KV head this query head reads
        let q_base = qh_idx * head_dim;
        let kv_base = kv * head_dim;

        let mut qh: Vec<Vec<f64>> = (0..seq)
            .map(|t| q[t][q_base..q_base + head_dim].to_vec())
            .collect();
        let mut kh: Vec<Vec<f64>> = (0..seq)
            .map(|t| k[t][kv_base..kv_base + head_dim].to_vec())
            .collect();
        for t in 0..seq {
            apply_rope(&mut qh[t], &cos[t], &sin[t]);
            apply_rope(&mut kh[t], &cos[t], &sin[t]);
        }

        for t in 0..seq {
            let mut scores = Vec::with_capacity(t + 1);
            for s in 0..=t {
                let dot: f64 = qh[t].iter().zip(kh[s].iter()).map(|(a, b)| a * b).sum();
                scores.push(dot * scale);
            }
            let weights = softmax(&scores);
            for (s, w) in weights.iter().enumerate() {
                for d in 0..head_dim {
                    out[t][q_base + d] += w * v[s][kv_base + d];
                }
            }
        }
    }
    out
}

/// Per-layer K/V cache for incremental decoding: each entry holds the rotated
/// key and the value for one past position, so a new token only computes its own
/// row and attends over the cache instead of recomputing the whole prefix.
#[derive(Default, Clone)]
pub struct KvCache {
    /// keys[pos] = rotated key vector (n_kv_heads * head_dim) at that position.
    pub keys: Vec<Vec<f64>>,
    pub values: Vec<Vec<f64>>,
}

/// One incremental GQA step: `q_new`/`k_new`/`v_new` are the single new token's
/// projections (length n_q_heads*head_dim and n_kv_heads*head_dim). Applies RoPE
/// using `cos`/`sin` (precomputed for the new token's position), appends k/v to
/// the cache, and returns the new token's context vector (n_q_heads*head_dim)
/// attending over the whole cache. The cache
/// grows by one row per call — turning the O(seq) per-token attention into O(1)
/// new work plus a scan over the cache.
pub fn gqa_step(
    q_new: &[f64],
    k_new: &[f64],
    v_new: &[f64],
    cache: &mut KvCache,
    n_q_heads: usize,
    n_kv_heads: usize,
    head_dim: usize,
    cos: &[f64],
    sin: &[f64],
) -> Vec<f64> {
    let scale = 1.0 / (head_dim as f64).sqrt();
    let group = n_q_heads / n_kv_heads;

    // Rotate the new key per KV head and append k/v to the cache.
    let mut k_rot = k_new.to_vec();
    for kv in 0..n_kv_heads {
        let b = kv * head_dim;
        apply_rope(&mut k_rot[b..b + head_dim], cos, sin);
    }
    cache.keys.push(k_rot);
    cache.values.push(v_new.to_vec());
    let t = cache.keys.len() - 1; // index of this position

    // Rotate the new query per Q head.
    let mut q_rot = q_new.to_vec();
    for qh in 0..n_q_heads {
        let b = qh * head_dim;
        apply_rope(&mut q_rot[b..b + head_dim], cos, sin);
    }

    let mut out = vec![0.0; n_q_heads * head_dim];
    for qh in 0..n_q_heads {
        let kv = qh / group;
        let q_base = qh * head_dim;
        let kv_base = kv * head_dim;
        // Scores against every cached key up to and including this position.
        let mut scores = Vec::with_capacity(t + 1);
        for s in 0..=t {
            let dot: f64 = (0..head_dim)
                .map(|d| q_rot[q_base + d] * cache.keys[s][kv_base + d])
                .sum();
            scores.push(dot * scale);
        }
        let weights = softmax(&scores);
        for (s, w) in weights.iter().enumerate() {
            for d in 0..head_dim {
                out[q_base + d] += w * cache.values[s][kv_base + d];
            }
        }
    }
    out
}

/// `x · w + bias` per row. `w` is `in_dim x out_dim`; `bias` length `out_dim`
/// (empty for no bias).
pub fn linear(x: &[Vec<f64>], w: &[Vec<f64>], bias: &[f64]) -> anyhow::Result<Matrix> {
    let mut out = crate::matrix::multiply_parallel(x, w)?;
    if !bias.is_empty() {
        for row in &mut out {
            for (v, b) in row.iter_mut().zip(bias.iter()) {
                *v += b;
            }
        }
    }
    Ok(out)
}

/// Multi-head causal self-attention for a single sequence (batch = 1).
///
/// `q`, `k`, `v` are `seq_len x (n_heads * head_dim)` (already projected). RoPE
/// is applied to q and k per head and position. Returns `seq_len x (n_heads *
/// head_dim)`: the concatenated per-head context vectors, ready for the output
/// projection. Causal masking lets each token attend only to itself and earlier
/// tokens.
pub fn causal_self_attention(
    q: &[Vec<f64>],
    k: &[Vec<f64>],
    v: &[Vec<f64>],
    n_heads: usize,
    head_dim: usize,
    cos: &[Vec<f64>],
    sin: &[Vec<f64>],
) -> Matrix {
    let seq = q.len();
    let scale = 1.0 / (head_dim as f64).sqrt();
    let mut out = vec![vec![0.0; n_heads * head_dim]; seq];

    for h in 0..n_heads {
        let base = h * head_dim;
        // Rotate this head's q/k for every position up front.
        let mut qh: Vec<Vec<f64>> = (0..seq)
            .map(|t| q[t][base..base + head_dim].to_vec())
            .collect();
        let mut kh: Vec<Vec<f64>> = (0..seq)
            .map(|t| k[t][base..base + head_dim].to_vec())
            .collect();
        for t in 0..seq {
            apply_rope(&mut qh[t], &cos[t], &sin[t]);
            apply_rope(&mut kh[t], &cos[t], &sin[t]);
        }

        for t in 0..seq {
            // Scores against all keys up to t (causal), then softmax, then
            // weighted sum of values.
            let mut scores = Vec::with_capacity(t + 1);
            for s in 0..=t {
                let dot: f64 = qh[t].iter().zip(kh[s].iter()).map(|(a, b)| a * b).sum();
                scores.push(dot * scale);
            }
            let weights = softmax(&scores);
            for (s, w) in weights.iter().enumerate() {
                for d in 0..head_dim {
                    out[t][base + d] += w * v[s][base + d];
                }
            }
        }
    }
    out
}

/// SwiGLU feed-forward: `(silu(x · w_gate) ⊙ (x · w_up)) · w_down`. `w_gate` and
/// `w_up` are `dim x hidden`, `w_down` is `hidden x dim`. This is the FFN
/// Llama/DeepSeek use in place of the classic two-layer ReLU MLP.
pub fn swiglu_ffn(
    x: &[Vec<f64>],
    w_gate: &[Vec<f64>],
    w_up: &[Vec<f64>],
    w_down: &[Vec<f64>],
) -> anyhow::Result<Matrix> {
    let gate = crate::matrix::multiply_parallel(x, w_gate)?;
    let up = crate::matrix::multiply_parallel(x, w_up)?;
    let hidden: Matrix = gate
        .iter()
        .zip(up.iter())
        .map(|(g_row, u_row)| {
            g_row
                .iter()
                .zip(u_row.iter())
                .map(|(g, u)| silu(*g) * u)
                .collect()
        })
        .collect();
    crate::matrix::multiply_parallel(&hidden, w_down)
}

/// Element-wise add (residual connection).
pub fn add(a: &[Vec<f64>], b: &[Vec<f64>]) -> Matrix {
    a.iter()
        .zip(b.iter())
        .map(|(ra, rb)| ra.iter().zip(rb.iter()).map(|(x, y)| x + y).collect())
        .collect()
}

/// The weights of one Llama/DeepSeek transformer block. Projection weights are
/// stored as `in_dim x out_dim` (so `x · w` works directly). Norm weights are
/// per-channel scales.
pub struct BlockWeights {
    pub attn_norm: Vec<f64>,
    pub wq: Matrix,
    pub wk: Matrix,
    pub wv: Matrix,
    pub wo: Matrix,
    pub ffn_norm: Vec<f64>,
    pub w_gate: Matrix,
    pub w_up: Matrix,
    pub w_down: Matrix,
}

/// Hyper-parameters of a transformer block.
#[derive(Clone, Copy)]
pub struct BlockConfig {
    pub n_heads: usize,
    pub head_dim: usize,
    pub rope_theta: f64,
    pub eps: f64,
}

/// Run one transformer block over a sequence (batch = 1):
///   h = x + Wo · attention(RoPE(Q,K), V)         where Q,K,V = norm(x)·Wq,k,v
///   out = h + SwiGLU(norm(h))
/// This is the unit a real model stacks N times. Pre-norm, residual, RoPE,
/// SwiGLU — the Llama/DeepSeek recipe.
pub fn transformer_block(
    x: &[Vec<f64>],
    w: &BlockWeights,
    cfg: BlockConfig,
) -> anyhow::Result<Matrix> {
    let seq = x.len();
    let normed = rms_norm(x, &w.attn_norm, cfg.eps);
    let q = crate::matrix::multiply_parallel(&normed, &w.wq)?;
    let k = crate::matrix::multiply_parallel(&normed, &w.wk)?;
    let v = crate::matrix::multiply_parallel(&normed, &w.wv)?;

    let (cos, sin) = rope_tables(seq, cfg.head_dim, cfg.rope_theta);
    let context = causal_self_attention(&q, &k, &v, cfg.n_heads, cfg.head_dim, &cos, &sin);
    let attn_out = crate::matrix::multiply_parallel(&context, &w.wo)?;
    let h = add(x, &attn_out);

    let ffn_in = rms_norm(&h, &w.ffn_norm, cfg.eps);
    let ffn_out = swiglu_ffn(&ffn_in, &w.w_gate, &w.w_up, &w.w_down)?;
    Ok(add(&h, &ffn_out))
}
