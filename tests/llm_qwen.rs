// Validates the full LLM forward against a real Qwen2.5-0.5B checkpoint.
// Ignored by default (needs the model downloaded to the HF cache); run with:
//   cargo test --test llm_qwen -- --ignored --nocapture
use litek8s::kv_cache::{DiskKvCache, DiskKvConfig};
use litek8s::llm::{GgufLlmWeights, LlmConfig, LlmWeights, MappedLlmWeights, argmax};
use std::path::PathBuf;

fn qwen_dir() -> Option<PathBuf> {
    let base = dirs_home()?.join(".cache/huggingface/hub/models--Qwen--Qwen2.5-0.5B/snapshots");
    let snap = std::fs::read_dir(&base).ok()?.flatten().next()?.path();
    Some(snap)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[test]
#[ignore]
fn predicts_paris() {
    let dir = qwen_dir().expect("Qwen2.5-0.5B must be in the HF cache");
    let cfg = LlmConfig::load(&dir.join("config.json")).unwrap();
    let weights = LlmWeights::load(&dir.join("model.safetensors"), cfg).unwrap();

    // "The capital of France is" -> these token ids (from the HF tokenizer).
    let tokens = [785u32, 6722, 315, 9625, 374];
    let logits = weights.forward_last_logits(&tokens).unwrap();
    let next = argmax(&logits);
    // Reference (transformers): argmax is 12095 = " Paris".
    assert_eq!(next, 12095, "expected ' Paris' (12095), got {next}");
}

#[test]
#[ignore]
fn mmap_backend_predicts_paris_without_resident_weights() {
    let dir = qwen_dir().expect("Qwen2.5-0.5B must be in the HF cache");
    let cfg = LlmConfig::load(&dir.join("config.json")).unwrap();
    let weights = MappedLlmWeights::load(&dir.join("model.safetensors"), cfg).unwrap();
    let tokens = [785u32, 6722, 315, 9625, 374];
    let logits = weights.forward_last_logits(&tokens).unwrap();
    let next = argmax(&logits);
    assert_eq!(next, 12095, "expected ' Paris' (12095), got {next}");
}

#[test]
#[ignore]
fn gguf_mmap_backend_predicts_paris_from_ollama_weights() {
    let dir = qwen_dir().expect("Qwen2.5-0.5B must be in the HF cache");
    let gguf = std::env::var_os("LITEK8S_QWEN_GGUF")
        .map(PathBuf::from)
        .expect("set LITEK8S_QWEN_GGUF to the qwen2.5:0.5b Ollama model blob");
    let cfg = LlmConfig::load(&dir.join("config.json")).unwrap();
    let weights = GgufLlmWeights::load(&gguf, cfg).unwrap();
    let tokens = [785u32, 6722, 315, 9625, 374];
    let logits = weights.forward_last_logits(&tokens).unwrap();
    let next = argmax(&logits);
    assert_eq!(next, 12095, "expected ' Paris' (12095), got {next}");
}

#[test]
#[ignore]
fn gguf_disk_kv_prefill_predicts_paris() {
    let dir = qwen_dir().expect("Qwen2.5-0.5B must be in the HF cache");
    let gguf = std::env::var_os("LITEK8S_QWEN_GGUF")
        .map(PathBuf::from)
        .expect("set LITEK8S_QWEN_GGUF to the qwen2.5:0.5b Ollama model blob");
    let cfg = LlmConfig::load(&dir.join("config.json")).unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let mut cache = DiskKvCache::create(DiskKvConfig {
        directories: vec![cache_dir.path().into()],
        num_layers: cfg.num_hidden_layers,
        num_kv_heads: cfg.num_key_value_heads,
        head_dim: cfg.head_dim(),
        max_tokens: 16,
        buffer_bytes: 1024 * 1024,
        keep_files: false,
    })
    .unwrap();
    let weights = GgufLlmWeights::load(&gguf, cfg).unwrap();
    let tokens = [785u32, 6722, 315, 9625, 374];
    let logits = weights.prefill_last_logits(&tokens, &mut cache).unwrap();
    let next = argmax(&logits);
    assert_eq!(next, 12095, "expected ' Paris' (12095), got {next}");
    assert_eq!(cache.stats().tokens, tokens.len());
}
