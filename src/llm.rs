//! Run a real Qwen2-architecture LLM (RMSNorm + GQA with QKV bias + RoPE +
//! SwiGLU, tied embeddings) from a safetensors checkpoint. Single-machine f64
//! for correctness first; the per-layer matmuls are the same column-shardable
//! ops the cluster already runs, so distribution slots in afterward.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::matrix::Matrix;
use crate::model;
use crate::transformer::{add, grouped_query_attention, linear, rms_norm, rope_tables, silu};

/// Qwen2 config (subset we need), parsed from the model's `config.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub num_key_value_heads: usize,
    pub intermediate_size: usize,
    pub vocab_size: usize,
    pub rms_norm_eps: f64,
    pub rope_theta: f64,
}

impl LlmConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        serde_json::from_str(&raw).context("failed to parse model config.json")
    }

    pub fn head_dim(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

/// Transpose an `out x in` HF weight into the `in x out` layout `x · w` wants.
fn transposed(m: &[Vec<f64>]) -> Matrix {
    if m.is_empty() {
        return Vec::new();
    }
    let rows = m.len();
    let cols = m[0].len();
    (0..cols)
        .map(|j| (0..rows).map(|i| m[i][j]).collect())
        .collect()
}

/// All weights of a Qwen2 model, loaded into RAM as f64 (projection weights
/// pre-transposed to `in x out`).
pub struct LlmWeights {
    cfg: LlmConfig,
    embed: Matrix, // vocab x hidden (also the tied lm_head)
    layers: Vec<LayerWeights>,
    final_norm: Vec<f64>,
}

struct LayerWeights {
    attn_norm: Vec<f64>,
    wq: Matrix,
    bq: Vec<f64>,
    wk: Matrix,
    bk: Vec<f64>,
    wv: Matrix,
    bv: Vec<f64>,
    wo: Matrix,
    ffn_norm: Vec<f64>,
    w_gate: Matrix,
    w_up: Matrix,
    w_down: Matrix,
}

impl LlmWeights {
    /// Load every tensor from the safetensors file, transposing projections.
    pub fn load(model_path: &Path, cfg: LlmConfig) -> Result<Self> {
        let load = |name: &str| model::load_tensor_2d(model_path, name);
        let bias = |name: &str| -> Result<Vec<f64>> {
            // Bias tensors are 1D; load_tensor_2d only does 2D, so read via a
            // 1-row trick: HF stores them as shape [n]; fall back to empty if
            // absent.
            match model::load_tensor_1d(model_path, name) {
                Ok(v) => Ok(v),
                Err(_) => Ok(Vec::new()),
            }
        };

        let embed = load("model.embed_tokens.weight")?; // vocab x hidden
        let final_norm = model::load_tensor_1d(model_path, "model.norm.weight")?;

        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let p = format!("model.layers.{i}");
            layers.push(LayerWeights {
                attn_norm: model::load_tensor_1d(
                    model_path,
                    &format!("{p}.input_layernorm.weight"),
                )?,
                wq: transposed(&load(&format!("{p}.self_attn.q_proj.weight"))?),
                bq: bias(&format!("{p}.self_attn.q_proj.bias"))?,
                wk: transposed(&load(&format!("{p}.self_attn.k_proj.weight"))?),
                bk: bias(&format!("{p}.self_attn.k_proj.bias"))?,
                wv: transposed(&load(&format!("{p}.self_attn.v_proj.weight"))?),
                bv: bias(&format!("{p}.self_attn.v_proj.bias"))?,
                wo: transposed(&load(&format!("{p}.self_attn.o_proj.weight"))?),
                ffn_norm: model::load_tensor_1d(
                    model_path,
                    &format!("{p}.post_attention_layernorm.weight"),
                )?,
                w_gate: transposed(&load(&format!("{p}.mlp.gate_proj.weight"))?),
                w_up: transposed(&load(&format!("{p}.mlp.up_proj.weight"))?),
                w_down: transposed(&load(&format!("{p}.mlp.down_proj.weight"))?),
            });
        }

        Ok(Self {
            cfg,
            embed,
            layers,
            final_norm,
        })
    }

    pub fn config(&self) -> &LlmConfig {
        &self.cfg
    }

    /// Forward a token-id sequence and return the logits over the vocab for the
    /// last position (what you sample the next token from).
    pub fn forward_last_logits(&self, tokens: &[u32]) -> Result<Vec<f64>> {
        let cfg = &self.cfg;
        let head_dim = cfg.head_dim();
        let seq = tokens.len();

        // Embedding lookup: each token id selects its row of the embedding table.
        let mut h: Matrix = tokens
            .iter()
            .map(|&t| self.embed[t as usize].clone())
            .collect();

        let (cos, sin) = rope_tables(seq, head_dim, cfg.rope_theta);

        for layer in &self.layers {
            let normed = rms_norm(&h, &layer.attn_norm, cfg.rms_norm_eps);
            let q = linear(&normed, &layer.wq, &layer.bq)?;
            let k = linear(&normed, &layer.wk, &layer.bk)?;
            let v = linear(&normed, &layer.wv, &layer.bv)?;
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
            let attn_out = linear(&context, &layer.wo, &[])?;
            h = add(&h, &attn_out);

            let ffn_in = rms_norm(&h, &layer.ffn_norm, cfg.rms_norm_eps);
            let gate = linear(&ffn_in, &layer.w_gate, &[])?;
            let up = linear(&ffn_in, &layer.w_up, &[])?;
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
            let ffn_out = linear(&hidden, &layer.w_down, &[])?;
            h = add(&h, &ffn_out);
        }

        // Final norm, then project the last token through the tied embedding to
        // get vocab logits.
        let normed = rms_norm(&h, &self.final_norm, cfg.rms_norm_eps);
        let last = &normed[seq - 1];
        let logits = (0..cfg.vocab_size)
            .map(|t| {
                self.embed[t]
                    .iter()
                    .zip(last.iter())
                    .map(|(w, x)| w * x)
                    .sum()
            })
            .collect();
        Ok(logits)
    }
}

/// Index of the largest logit (greedy next-token).
pub fn argmax(logits: &[f64]) -> usize {
    logits
        .iter()
        .enumerate()
        .fold((0, f64::NEG_INFINITY), |(bi, bv), (i, &v)| {
            if v > bv { (i, v) } else { (bi, bv) }
        })
        .0
}

/// A loaded tokenizer (HF `tokenizer.json`) plus the model, for text in / text
/// out. Wraps the `tokenizers` crate.
pub struct LlmRunner {
    weights: LlmWeights,
    tokenizer: tokenizers::Tokenizer,
}

impl LlmRunner {
    /// Load a model directory containing `config.json`, `model.safetensors`, and
    /// `tokenizer.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let cfg = LlmConfig::load(&dir.join("config.json"))?;
        let weights = LlmWeights::load(&dir.join("model.safetensors"), cfg)?;
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;
        Ok(Self { weights, tokenizer })
    }

    pub fn weights(&self) -> &LlmWeights {
        &self.weights
    }

    /// Greedy autoregressive generation: encode the prompt, then append the
    /// argmax token `max_new` times (re-running the full sequence each step — no
    /// KV cache yet, so cost grows with length). `on_token` is called with each
    /// new token's decoded text for streaming. Returns the full decoded output.
    pub fn generate(
        &self,
        prompt: &str,
        max_new: usize,
        mut on_token: impl FnMut(&str),
    ) -> Result<String> {
        let encoding = self
            .tokenizer
            .encode(prompt, false)
            .map_err(|e| anyhow::anyhow!("tokenize failed: {e}"))?;
        let mut tokens: Vec<u32> = encoding.get_ids().to_vec();
        let start = tokens.len();

        for _ in 0..max_new {
            let logits = self.weights.forward_last_logits(&tokens)?;
            let next = argmax(&logits) as u32;
            tokens.push(next);
            // Decode just the new token for streaming output.
            if let Ok(piece) = self.tokenizer.decode(&[next], false) {
                on_token(&piece);
            }
        }

        self.tokenizer
            .decode(&tokens[start..], false)
            .map_err(|e| anyhow::anyhow!("decode failed: {e}"))
    }
}
