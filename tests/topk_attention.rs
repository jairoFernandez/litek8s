// Empirical check of the "geometric" attention hypothesis: does the softmax
// concentrate its mass on a few keys, so attending only over the top-k
// highest-scoring positions preserves generation?
//
// Ignored by default (needs the Ollama GGUF blob); run with:
//   LITEK8S_QWEN_GGUF=... cargo test --release --test topk_attention -- --ignored --nocapture
use litek8s::gguf::GgufModel;
use litek8s::llm::{LlmConfig, argmax};
use litek8s::transformer::{add, apply_rope, rms_norm, rope_at, silu, softmax};
use std::path::PathBuf;

const PROMPT: [u32; 5] = [785, 6722, 315, 9625, 374]; // "The capital of France is"
const NEW_TOKENS: usize = 96;
const MASS_KS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
const MIN_CONTEXT_FOR_STATS: usize = 32;

fn qwen_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let base = home.join(".cache/huggingface/hub/models--Qwen--Qwen2.5-0.5B/snapshots");
    Some(std::fs::read_dir(&base).ok()?.flatten().next()?.path())
}

fn add_bias(rows: &mut [Vec<f64>], bias: &[f64]) {
    for row in rows {
        for (value, b) in row.iter_mut().zip(bias) {
            *value += b;
        }
    }
}

struct LayerMeta {
    prefix: String,
    attn_norm: Vec<f64>,
    bq: Vec<f64>,
    bk: Vec<f64>,
    bv: Vec<f64>,
    ffn_norm: Vec<f64>,
}

/// In-memory rotated K / V rows for one layer.
#[derive(Default, Clone)]
struct LayerKv {
    keys: Vec<Vec<f64>>,
    values: Vec<Vec<f64>>,
}

/// Softmax-mass accumulator: how much probability the top-k keys capture.
#[derive(Default)]
struct MassStats {
    sums: [f64; MASS_KS.len()],
    mins: [f64; MASS_KS.len()],
    count: usize,
}

impl MassStats {
    fn new() -> Self {
        Self {
            sums: [0.0; MASS_KS.len()],
            mins: [1.0; MASS_KS.len()],
            count: 0,
        }
    }

    fn record(&mut self, weights: &[f64]) {
        let mut sorted = weights.to_vec();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
        for (slot, &k) in MASS_KS.iter().enumerate() {
            let mass: f64 = sorted.iter().take(k).sum();
            self.sums[slot] += mass;
            if mass < self.mins[slot] {
                self.mins[slot] = mass;
            }
        }
        self.count += 1;
    }
}

enum Attention<'a> {
    Full(Option<&'a mut MassStats>),
    TopK(usize),
}

/// One attention step over the in-memory cache. Appends the rotated key and the
/// value, then attends with either the full softmax or a renormalized top-k.
#[allow(clippy::too_many_arguments)]
fn attention_step(
    kv: &mut LayerKv,
    q_new: &[f64],
    k_new: &[f64],
    v_new: &[f64],
    n_q_heads: usize,
    n_kv_heads: usize,
    head_dim: usize,
    cos: &[f64],
    sin: &[f64],
    mode: &mut Attention,
) -> Vec<f64> {
    let scale = 1.0 / (head_dim as f64).sqrt();
    let group = n_q_heads / n_kv_heads;

    let mut key = k_new.to_vec();
    for kv_head in 0..n_kv_heads {
        let base = kv_head * head_dim;
        apply_rope(&mut key[base..base + head_dim], cos, sin);
    }
    kv.keys.push(key);
    kv.values.push(v_new.to_vec());

    let mut query = q_new.to_vec();
    for q_head in 0..n_q_heads {
        let base = q_head * head_dim;
        apply_rope(&mut query[base..base + head_dim], cos, sin);
    }

    let sequence = kv.keys.len();
    let mut output = vec![0.0; n_q_heads * head_dim];
    for q_head in 0..n_q_heads {
        let kv_head = q_head / group;
        let q_base = q_head * head_dim;
        let kv_base = kv_head * head_dim;
        let scores: Vec<f64> = (0..sequence)
            .map(|s| {
                let dot: f64 = (0..head_dim)
                    .map(|d| query[q_base + d] * kv.keys[s][kv_base + d])
                    .sum();
                dot * scale
            })
            .collect();

        match mode {
            Attention::Full(stats) => {
                let weights = softmax(&scores);
                if let Some(stats) = stats {
                    if sequence >= MIN_CONTEXT_FOR_STATS {
                        stats.record(&weights);
                    }
                }
                for (s, w) in weights.iter().enumerate() {
                    for d in 0..head_dim {
                        output[q_base + d] += w * kv.values[s][kv_base + d];
                    }
                }
            }
            Attention::TopK(k) => {
                let keep = (*k).min(sequence);
                let mut order: Vec<usize> = (0..sequence).collect();
                order.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap());
                let selected = &order[..keep];
                let kept_scores: Vec<f64> = selected.iter().map(|&s| scores[s]).collect();
                let weights = softmax(&kept_scores);
                for (&s, w) in selected.iter().zip(weights.iter()) {
                    for d in 0..head_dim {
                        output[q_base + d] += w * kv.values[s][kv_base + d];
                    }
                }
            }
        }
    }
    output
}

struct Runner {
    model: GgufModel,
    cfg: LlmConfig,
    layers: Vec<LayerMeta>,
    final_norm: Vec<f64>,
}

impl Runner {
    fn load() -> Self {
        let dir = qwen_dir().expect("Qwen2.5-0.5B must be in the HF cache");
        let gguf = std::env::var_os("LITEK8S_QWEN_GGUF")
            .map(PathBuf::from)
            .expect("set LITEK8S_QWEN_GGUF to the qwen2.5:0.5b Ollama model blob");
        let cfg = LlmConfig::load(&dir.join("config.json")).unwrap();
        let model = GgufModel::open(&gguf).unwrap();
        let final_norm = model.tensor_1d("output_norm.weight").unwrap();
        let layers = (0..cfg.num_hidden_layers)
            .map(|index| {
                let prefix = format!("blk.{index}");
                LayerMeta {
                    attn_norm: model.tensor_1d(&format!("{prefix}.attn_norm.weight")).unwrap(),
                    bq: model.tensor_1d(&format!("{prefix}.attn_q.bias")).unwrap(),
                    bk: model.tensor_1d(&format!("{prefix}.attn_k.bias")).unwrap(),
                    bv: model.tensor_1d(&format!("{prefix}.attn_v.bias")).unwrap(),
                    ffn_norm: model.tensor_1d(&format!("{prefix}.ffn_norm.weight")).unwrap(),
                    prefix,
                }
            })
            .collect();
        Self {
            model,
            cfg,
            layers,
            final_norm,
        }
    }

    /// Forward `tokens` (appending to `caches`) and return the last position's
    /// logits. `positions` are the absolute positions of `tokens`.
    fn forward(
        &self,
        tokens: &[u32],
        first_position: usize,
        caches: &mut [LayerKv],
        mode: &mut Attention,
    ) -> Vec<f64> {
        let cfg = &self.cfg;
        let head_dim = cfg.head_dim();
        let rows: Vec<usize> = tokens.iter().map(|&t| t as usize).collect();
        let mut hidden = self.model.tensor_rows("token_embd.weight", &rows).unwrap();

        for (layer_index, layer) in self.layers.iter().enumerate() {
            let normed = rms_norm(&hidden, &layer.attn_norm, cfg.rms_norm_eps);
            let mut query = self
                .model
                .linear_mapped(&format!("{}.attn_q.weight", layer.prefix), &normed)
                .unwrap();
            let mut key = self
                .model
                .linear_mapped(&format!("{}.attn_k.weight", layer.prefix), &normed)
                .unwrap();
            let mut value = self
                .model
                .linear_mapped(&format!("{}.attn_v.weight", layer.prefix), &normed)
                .unwrap();
            add_bias(&mut query, &layer.bq);
            add_bias(&mut key, &layer.bk);
            add_bias(&mut value, &layer.bv);

            let mut context = Vec::with_capacity(tokens.len());
            for offset in 0..tokens.len() {
                let (cos, sin) = rope_at(first_position + offset, head_dim, cfg.rope_theta);
                context.push(attention_step(
                    &mut caches[layer_index],
                    &query[offset],
                    &key[offset],
                    &value[offset],
                    cfg.num_attention_heads,
                    cfg.num_key_value_heads,
                    head_dim,
                    &cos,
                    &sin,
                    mode,
                ));
            }
            let attention = self
                .model
                .linear_mapped(&format!("{}.attn_output.weight", layer.prefix), &context)
                .unwrap();
            hidden = add(&hidden, &attention);

            let ffn_input = rms_norm(&hidden, &layer.ffn_norm, cfg.rms_norm_eps);
            let gate = self
                .model
                .linear_mapped(&format!("{}.ffn_gate.weight", layer.prefix), &ffn_input)
                .unwrap();
            let up = self
                .model
                .linear_mapped(&format!("{}.ffn_up.weight", layer.prefix), &ffn_input)
                .unwrap();
            let activated: Vec<Vec<f64>> = gate
                .iter()
                .zip(up.iter())
                .map(|(g_row, u_row)| {
                    g_row
                        .iter()
                        .zip(u_row)
                        .map(|(g, u)| silu(*g) * u)
                        .collect()
                })
                .collect();
            let ffn = self
                .model
                .linear_mapped(&format!("{}.ffn_down.weight", layer.prefix), &activated)
                .unwrap();
            hidden = add(&hidden, &ffn);
        }

        let normed = rms_norm(&hidden, &self.final_norm, cfg.rms_norm_eps);
        self.model
            .row_logits("token_embd.weight", &normed[tokens.len() - 1])
            .unwrap()
    }

    /// Greedy generation. Returns the generated token ids.
    fn generate(&self, mode: &mut Attention) -> Vec<u32> {
        let mut caches = vec![LayerKv::default(); self.cfg.num_hidden_layers];
        let mut logits = self.forward(&PROMPT, 0, &mut caches, mode);
        let mut generated = Vec::with_capacity(NEW_TOKENS);
        for step in 0..NEW_TOKENS {
            let next = argmax(&logits) as u32;
            generated.push(next);
            if step + 1 == NEW_TOKENS {
                break;
            }
            logits = self.forward(&[next], PROMPT.len() + step, &mut caches, mode);
        }
        generated
    }
}

#[test]
#[ignore]
fn topk_attention_preserves_generation() {
    let runner = Runner::load();

    let mut stats = MassStats::new();
    let mut full_mode = Attention::Full(Some(&mut stats));
    let start = std::time::Instant::now();
    let baseline = runner.generate(&mut full_mode);
    let full_elapsed = start.elapsed();

    println!("\nfull attention: {NEW_TOKENS} tokens in {full_elapsed:.2?}");
    println!("first 12 ids: {:?}", &baseline[..12.min(baseline.len())]);
    println!(
        "\nsoftmax mass captured by top-k keys ({} head-steps, context >= {}):",
        stats.count, MIN_CONTEXT_FOR_STATS
    );
    println!("{:>6} {:>10} {:>10}", "k", "mean", "worst");
    for (slot, &k) in MASS_KS.iter().enumerate() {
        println!(
            "{k:>6} {:>9.4} {:>9.4}",
            stats.sums[slot] / stats.count as f64,
            stats.mins[slot]
        );
    }

    for k in [8usize, 16, 32] {
        let start = std::time::Instant::now();
        let candidate = runner.generate(&mut Attention::TopK(k));
        let elapsed = start.elapsed();
        let matching = baseline
            .iter()
            .zip(candidate.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let total_equal = baseline == candidate;
        println!(
            "\ntop-{k}: {NEW_TOKENS} tokens in {elapsed:.2?}; identical prefix {matching}/{}, sequences equal: {total_equal}",
            baseline.len()
        );
        if !total_equal {
            println!(
                "  diverges at step {matching}: full={:?} topk={:?}",
                baseline.get(matching),
                candidate.get(matching)
            );
        }
    }

    // The hypothesis is validated if a small k already tracks the full softmax
    // closely; assert the loosest version so the numbers above stay the source
    // of truth.
    let k16_slot = MASS_KS.iter().position(|&k| k == 16).unwrap();
    let mean_mass_k16 = stats.sums[k16_slot] / stats.count as f64;
    assert!(
        mean_mass_k16 > 0.9,
        "top-16 keys captured only {mean_mass_k16:.4} of softmax mass on average"
    );
}
