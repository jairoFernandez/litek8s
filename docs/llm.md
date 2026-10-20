# Local LLM inference (experimental, `--features llm`)

The `llm` feature (on in `cargo build`'s defaults, off in the release
binaries) adds `litek8s llm` and `litek8s model`: Qwen inference on the CPU
with SafeTensors or GGUF weights, a disk-backed KV cache and distributed
matmul across nodes. It is an experiment next to the orchestrator, not part
of the Kubernetes path.

## The mmap backend

Local Qwen inference has three interchangeable weight readers:

- the default `resident` backend loads and transposes every projection as
  `f64`;
- `--mmap` keeps large tensors in their native SafeTensors dtype and streams
  them directly from the read-only mapping;
- `--gguf FILE` reads the mixed-quantized GGUF format used by Ollama directly
  through mmap. Q4_K_M is a recipe mixing Q4_K, Q5_0, Q6_K, Q8_0 and F32
  tensors; every supported block is dequantized inside the dot product.

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --mmap \
  --max-new 1 \
  "The capital of France is"
```

The same Ollama blob can be tested without copying or converting it:

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --gguf ~/.ollama/models/blobs/sha256-... \
  --max-new 1 \
  "The capital of France is"
```

Incremental generation can place the KV cache on one or more disks:

```bash
cargo run --release -- llm generate \
  --model-dir /path/to/Qwen2.5-0.5B \
  --gguf ~/.ollama/models/blobs/sha256-... \
  --kv-cache-dir /fast-disk/litek8s-kv,/large-disk/litek8s-kv \
  --kv-max-tokens 32768 \
  --kv-buffer-mb 16 \
  --max-new 4 \
  "The capital of France is"
```

This mode performs one batched prompt prefill and then processes only the new
token at each decode step. It creates one grow-on-demand F16 K/V file per
transformer layer, distributes the files round-robin across the supplied
directories, and accesses them through writable mmap. Files grow in 64-token
groups, avoiding full-capacity allocation on filesystems such as APFS. Only the
configured tile buffer and the current attention scores need additional
resident memory. Cache files are removed automatically; pass
`--keep-kv-cache` to retain them for inspection or reuse by external tooling.

For Qwen2.5-0.5B, disk KV storage costs 12 KiB per cached token across all 24
layers. A 32,768-token limit therefore has 384 MiB of maximum capacity, while
the initial mapped allocation is only 1.5 MiB across 24 layers. The CLI reports
both bytes containing live rows and the current mapped file lengths.

The mmap backend loads only requested embedding rows, decodes each projection
weight once per input batch, reuses it across the batch rows, and computes tied
embedding logits without materializing the vocabulary matrix. The original
backend remains the default so both paths can be compared.

Measured on Qwen2.5-0.5B for one generated token:

| backend | wall | maximum RSS | output |
|:---|---:|---:|:---|
| resident `f64` | 90.31 s | 4,750.7 MB | `Paris` |
| native mmap | 4.86 s | 1,071.4 MB | `Paris` |

RSS includes pages from the 942 MiB checkpoint mapping. The mapped backend is
exact at the checkpoint dtype. The GGUF backend instead accepts the model's
Q4_K_M quantization error in exchange for its smaller working set.

An alternating, page-cache-warm comparison isolates the two mmap readers:

| mmap backend | file size | wall (1 token) | maximum RSS | output |
|:---|---:|---:|---:|:---|
| SafeTensors BF16 | 988,097,824 B | 2.86 s | 1,073.5 MB | `Paris` |
| Ollama GGUF Q4_K_M | 397,807,936 B | 2.35 s | 476.3 MB | `Paris` |

The quantized file is 2.484x smaller, reduces measured RSS by about 55.6%, and
is about 1.22x faster in the current scalar CPU kernel. GGUF startup alone is
about 0.40 s versus 0.20 s for SafeTensors.

With the same GGUF, prompt, warm page cache, and four generated tokens, the disk
KV path produced the same `Paris. It is` output in 4.31 s versus 10.18 s for
full-prefix recomputation, a 2.36x speedup. It used 0.10 MiB of KV rows for the
eight positions actually processed. These timings describe the current scalar
CPU implementation; longer contexts trade additional disk I/O for bounded,
evictable memory instead of keeping the whole KV history in anonymous RAM.

Measured with `/usr/bin/time -l` on macOS, both modes peak at about 454 MiB of
RSS — dominated by the file-backed, evictable pages of the ~379 MiB mmapped
GGUF — while the anonymous "peak memory footprint" is 82 MiB for the disk KV
path versus 84 MiB for recomputation. At only eight cached positions the KV
difference is negligible; the disk path's advantage grows with context length,
since its resident KV state stays bounded by the tile buffer rather than the
full history.
