//! Reading model weights from safetensors files.
//!
//! safetensors is the de-facto format for LLM weights (HuggingFace): a JSON
//! header naming each tensor with its dtype, shape, and byte offsets, followed
//! by the contiguous tensor data. This module opens such a file, lists its
//! tensors, and decodes a 2D weight tensor into the `Matrix` (row-major f64)
//! the rest of litek8s computes on — upcasting f16/bf16/f32 to f64 so a single
//! kernel handles every common weight dtype.

use anyhow::{Context, Result, bail, ensure};
use half::{bf16, f16};
use safetensors::{Dtype, SafeTensors};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::matrix::Matrix;

/// A model distributed across the cluster: an ordered list of layers, each
/// sharded by columns across nodes. The full weights never live in one place —
/// each node holds only its shards (cached on disk), and this manifest records
/// who has what so inference can route activations without re-reading the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelManifest {
    pub name: String,
    pub layers: Vec<LayerManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayerManifest {
    pub index: usize,
    pub tensor: String,
    pub in_dim: usize,
    pub out_dim: usize,
    pub relu: bool,
    pub shards: Vec<ShardManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShardManifest {
    pub node_name: String,
    pub weight_id: String,
    pub col_start: usize,
    pub cols: usize,
}

impl ModelManifest {
    /// Width the model expects its input activation to have (first layer in_dim).
    pub fn input_dim(&self) -> Option<usize> {
        self.layers.first().map(|l| l.in_dim)
    }
}

/// One tensor's metadata as reported by a safetensors header.
#[derive(Debug, Clone)]
pub struct TensorInfo {
    pub name: String,
    pub dtype: String,
    pub shape: Vec<usize>,
}

/// List every tensor in a safetensors file without decoding any data — reads
/// only the header, so it is cheap even for multi-GB models.
pub fn inspect(path: &Path) -> Result<Vec<TensorInfo>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read model file {}", path.display()))?;
    let tensors = SafeTensors::deserialize(&bytes)
        .with_context(|| format!("failed to parse safetensors file {}", path.display()))?;

    let mut infos: Vec<TensorInfo> = tensors
        .iter()
        .map(|(name, view)| TensorInfo {
            name: name.to_string(),
            dtype: format!("{:?}", view.dtype()),
            shape: view.shape().to_vec(),
        })
        .collect();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(infos)
}

/// Load a named 2D tensor from a safetensors file as a row-major f64 `Matrix`.
/// Upcasts f16/bf16/f32 to f64; rejects non-2D tensors and unsupported dtypes.
pub fn load_tensor_2d(path: &Path, name: &str) -> Result<Matrix> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read model file {}", path.display()))?;
    let tensors = SafeTensors::deserialize(&bytes)
        .with_context(|| format!("failed to parse safetensors file {}", path.display()))?;
    let view = tensors
        .tensor(name)
        .with_context(|| format!("tensor `{name}` not found in {}", path.display()))?;

    let shape = view.shape();
    ensure!(
        shape.len() == 2,
        "tensor `{name}` is {}D; only 2D weight matrices are supported",
        shape.len()
    );
    let rows = shape[0];
    let cols = shape[1];

    let flat = decode_to_f64(view.dtype(), view.data())
        .with_context(|| format!("failed to decode tensor `{name}`"))?;
    ensure!(
        flat.len() == rows * cols,
        "tensor `{name}` data length {} does not match shape {rows}x{cols}",
        flat.len()
    );

    Ok(flat
        .chunks_exact(cols)
        .map(|chunk| chunk.to_vec())
        .collect())
}

/// Read a 2D tensor and emit it as row-major f16 little-endian bytes, without
/// ever materializing the full f64 matrix — for the embedding table, which is
/// large (vocab x hidden) and would blow a small node's RAM at f64. Returns
/// (rows, cols, f16_bytes).
pub fn load_tensor_2d_f16(path: &Path, name: &str) -> Result<(usize, usize, Vec<u8>)> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read model file {}", path.display()))?;
    let tensors = SafeTensors::deserialize(&bytes)
        .with_context(|| format!("failed to parse safetensors file {}", path.display()))?;
    let view = tensors
        .tensor(name)
        .with_context(|| format!("tensor `{name}` not found in {}", path.display()))?;
    ensure!(view.shape().len() == 2, "tensor `{name}` is not 2D");
    let rows = view.shape()[0];
    let cols = view.shape()[1];

    // Convert element-by-element straight to f16 — never hold the full f64
    // matrix (1GB for a vocab x hidden embedding would blow a small node's RAM).
    let data = view.data();
    let dtype = view.dtype();
    let elem = dtype.size();
    ensure!(data.len() == rows * cols * elem, "tensor `{name}` shape mismatch");
    let mut out = Vec::with_capacity(rows * cols * 2);
    for chunk in data.chunks_exact(elem) {
        let v = decode_one(dtype, chunk);
        out.extend_from_slice(&half::f16::from_f64(v).to_le_bytes());
    }
    Ok((rows, cols, out))
}

/// Bytes-per-element for a safetensors float dtype (supported set).
trait DtypeSize {
    fn size(&self) -> usize;
}
impl DtypeSize for Dtype {
    fn size(&self) -> usize {
        match self {
            Dtype::F64 => 8,
            Dtype::F32 => 4,
            Dtype::F16 | Dtype::BF16 => 2,
            _ => 0,
        }
    }
}

/// Decode one little-endian element of a supported float dtype to f64.
fn decode_one(dtype: Dtype, b: &[u8]) -> f64 {
    match dtype {
        Dtype::F64 => f64::from_le_bytes(b.try_into().unwrap()),
        Dtype::F32 => f64::from(f32::from_le_bytes(b.try_into().unwrap())),
        Dtype::F16 => f64::from(f16::from_le_bytes(b.try_into().unwrap()).to_f32()),
        Dtype::BF16 => f64::from(bf16::from_le_bytes(b.try_into().unwrap()).to_f32()),
        _ => 0.0,
    }
}

/// One or more safetensors files mmap'd once. The OS pages tensor data in on
/// demand, so a multi-GB model never lands in RAM in full — essential on a small
/// coordinator that distributes a model larger than its memory. Supports both a
/// single `model.safetensors` and a sharded model (`*.index.json` + multiple
/// `model-0000N-of-...safetensors`).
pub struct ModelFile {
    mmaps: Vec<memmap2::Mmap>,
}

impl ModelFile {
    /// Open `path`. If a sibling `model.safetensors.index.json` exists, mmap all
    /// the shards it references; otherwise mmap the single file.
    pub fn open(path: &Path) -> Result<Self> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let index = dir.join("model.safetensors.index.json");
        let files: Vec<std::path::PathBuf> = if index.exists() {
            let raw = std::fs::read_to_string(&index)
                .with_context(|| format!("failed to read {}", index.display()))?;
            let parsed: serde_json::Value =
                serde_json::from_str(&raw).context("failed to parse safetensors index")?;
            let weight_map = parsed
                .get("weight_map")
                .and_then(|m| m.as_object())
                .context("index.json missing weight_map")?;
            let mut names: Vec<String> = weight_map
                .values()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            names.sort();
            names.dedup();
            names.into_iter().map(|n| dir.join(n)).collect()
        } else {
            vec![path.to_path_buf()]
        };

        let mut mmaps = Vec::with_capacity(files.len());
        for f in files {
            let file = std::fs::File::open(&f)
                .with_context(|| format!("failed to open model shard {}", f.display()))?;
            // SAFETY: read-only for the process lifetime; safetensors only reads.
            let mmap = unsafe { memmap2::Mmap::map(&file) }
                .with_context(|| format!("failed to mmap {}", f.display()))?;
            mmaps.push(mmap);
        }
        Ok(Self { mmaps })
    }

    /// Run `f` with the TensorView for `name`, searching every shard.
    fn with_view<T>(
        &self,
        name: &str,
        f: impl FnOnce(safetensors::tensor::TensorView<'_>) -> Result<T>,
    ) -> Result<T> {
        for mmap in &self.mmaps {
            let tensors = SafeTensors::deserialize(mmap)
                .context("failed to parse safetensors header")?;
            if let Ok(view) = tensors.tensor(name) {
                return f(view);
            }
        }
        anyhow::bail!("tensor `{name}` not found in any shard")
    }

    /// Load a 2D tensor as a row-major f64 Matrix.
    pub fn tensor_2d(&self, name: &str) -> Result<Matrix> {
        self.with_view(name, |view| {
            ensure!(view.shape().len() == 2, "tensor `{name}` is not 2D");
            let cols = view.shape()[1];
            let flat = decode_to_f64(view.dtype(), view.data())?;
            Ok(flat.chunks_exact(cols).map(|c| c.to_vec()).collect())
        })
    }

    /// Load a 1D tensor (norms, biases) as f64; empty vec if absent.
    pub fn tensor_1d(&self, name: &str) -> Vec<f64> {
        self.with_view(name, |view| {
            ensure!(view.shape().len() == 1, "not 1D");
            decode_to_f64(view.dtype(), view.data())
        })
        .unwrap_or_default()
    }

    /// Load a 2D tensor straight to row-major f16 bytes (no full f64 copy) — for
    /// the embedding table on a memory-tight node. Returns (rows, cols, bytes).
    pub fn tensor_2d_f16(&self, name: &str) -> Result<(usize, usize, Vec<u8>)> {
        self.with_view(name, |view| {
            ensure!(view.shape().len() == 2, "tensor `{name}` is not 2D");
            let (rows, cols) = (view.shape()[0], view.shape()[1]);
            let dtype = view.dtype();
            let elem = dtype.size();
            let data = view.data();
            let mut out = Vec::with_capacity(rows * cols * 2);
            for chunk in data.chunks_exact(elem) {
                out.extend_from_slice(&half::f16::from_f64(decode_one(dtype, chunk)).to_le_bytes());
            }
            Ok((rows, cols, out))
        })
    }
}

/// Map a safetensors dtype string to the cache's storage dtype, so weights keep
/// their native precision (f16/bf16) on disk and over the wire instead of being
/// inflated to f64. f32/f64 stay as-is; quantized integer formats are not
/// handled here (they reach this path already rejected by the decoder).
pub fn storage_dtype(dtype: &str) -> crate::matrix::BinDtype {
    match dtype {
        "F16" => crate::matrix::BinDtype::F16,
        "BF16" => crate::matrix::BinDtype::Bf16,
        "F32" => crate::matrix::BinDtype::F32,
        _ => crate::matrix::BinDtype::F64,
    }
}

/// Load a named 1D tensor (norm weights, biases) as a flat f64 vector,
/// upcasting f16/bf16/f32.
pub fn load_tensor_1d(path: &Path, name: &str) -> Result<Vec<f64>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read model file {}", path.display()))?;
    let tensors = SafeTensors::deserialize(&bytes)
        .with_context(|| format!("failed to parse safetensors file {}", path.display()))?;
    let view = tensors
        .tensor(name)
        .with_context(|| format!("tensor `{name}` not found in {}", path.display()))?;
    ensure!(
        view.shape().len() == 1,
        "tensor `{name}` is {}D; expected 1D",
        view.shape().len()
    );
    decode_to_f64(view.dtype(), view.data())
        .with_context(|| format!("failed to decode tensor `{name}`"))
}

/// Read a column band of a named 2D weight tensor as column-major rows — i.e.
/// `cols` vectors of length `in_dim`, ready to cache as a node's shard. Loads
/// one layer's tensor at a time (never the whole model), then slices out just
/// the requested columns.
pub fn load_layer_column_shard(
    path: &Path,
    tensor: &str,
    col_start: usize,
    cols: usize,
) -> Result<Matrix> {
    let weight = load_tensor_2d(path, tensor)?;
    let out_dim = weight.first().map(|r| r.len()).unwrap_or(0);
    ensure!(
        col_start + cols <= out_dim,
        "column band {col_start}..{} exceeds tensor `{tensor}` width {out_dim}",
        col_start + cols
    );
    // Transpose just the requested columns: result[j][i] = weight[i][col_start+j].
    Ok((0..cols)
        .map(|j| weight.iter().map(|row| row[col_start + j]).collect())
        .collect())
}

/// Decode a contiguous little-endian tensor payload into f64 values, upcasting
/// the common floating-point weight dtypes. Integer/quantized dtypes are
/// rejected for now — they need dequantization (a later step).
fn decode_to_f64(dtype: Dtype, data: &[u8]) -> Result<Vec<f64>> {
    match dtype {
        Dtype::F64 => decode_chunks::<8, _>(data, f64::from_le_bytes),
        Dtype::F32 => decode_chunks::<4, _>(data, |b| f64::from(f32::from_le_bytes(b))),
        Dtype::F16 => decode_chunks::<2, _>(data, |b| f64::from(f16::from_le_bytes(b).to_f32())),
        Dtype::BF16 => decode_chunks::<2, _>(data, |b| f64::from(bf16::from_le_bytes(b).to_f32())),
        other => bail!(
            "unsupported weight dtype {other:?}; supported: F64, F32, F16, BF16 \
             (quantized formats need dequantization first)"
        ),
    }
}

/// Split `data` into fixed-size little-endian elements and map each to an f64.
fn decode_chunks<const N: usize, F>(data: &[u8], convert: F) -> Result<Vec<f64>>
where
    F: Fn([u8; N]) -> f64,
{
    ensure!(
        data.len().is_multiple_of(N),
        "tensor payload length {} is not a multiple of element size {N}",
        data.len()
    );
    Ok(data
        .chunks_exact(N)
        .map(|chunk| convert(chunk.try_into().expect("chunk is exactly N bytes")))
        .collect())
}
