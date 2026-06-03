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
use std::path::Path;

use crate::matrix::Matrix;

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
