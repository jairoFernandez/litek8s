//! Zero-copy access to GGUF tensors used by llama.cpp/Ollama.
//!
//! The GGUF directory is parsed once, while tensor payloads stay in a read-only
//! mmap. Quantized rows are decoded block-by-block directly inside dot products;
//! no full dequantized copy of the model is ever allocated.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use half::f16;

use crate::matrix::Matrix;

const F32: u32 = 0;
const F16: u32 = 1;
const Q5_0: u32 = 6;
const Q8_0: u32 = 8;
const Q4_K: u32 = 12;
const Q6_K: u32 = 14;

#[derive(Debug, Clone)]
struct Tensor {
    kind: u32,
    shape: Vec<usize>,
    start: usize,
    len: usize,
}

/// A GGUF file whose tensor payload remains backed by the filesystem.
pub struct GgufModel {
    mmap: memmap2::Mmap,
    tensors: HashMap<String, Tensor>,
}

impl GgufModel {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("failed to open GGUF {}", path.display()))?;
        let indexed = parse_directory(&mut file)
            .with_context(|| format!("failed to parse GGUF directory {}", path.display()))?;
        // SAFETY: the mapping is read-only and owned for the lifetime of this object.
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("failed to mmap GGUF {}", path.display()))?;
        let data_offset =
            usize::try_from(indexed.data_offset).context("GGUF data offset overflow")?;
        let mut tensors = HashMap::with_capacity(indexed.tensors.len());
        for source in indexed.tensors {
            let relative = usize::try_from(source.offset).context("tensor offset overflow")?;
            let start = data_offset
                .checked_add(relative)
                .context("absolute tensor offset overflow")?;
            ensure!(
                start
                    .checked_add(source.len)
                    .is_some_and(|end| end <= mmap.len()),
                "tensor `{}` lies outside the GGUF file",
                source.name
            );
            ensure_supported(source.kind, &source.name)?;
            tensors.insert(
                source.name,
                Tensor {
                    kind: source.kind,
                    shape: source.shape,
                    start,
                    len: source.len,
                },
            );
        }
        Ok(Self { mmap, tensors })
    }

    fn tensor(&self, name: &str) -> Result<(&Tensor, &[u8])> {
        let tensor = self
            .tensors
            .get(name)
            .with_context(|| format!("tensor `{name}` not found in GGUF"))?;
        Ok((tensor, &self.mmap[tensor.start..tensor.start + tensor.len]))
    }

    /// Decode a one-dimensional norm or bias. These are normally F32 in GGUF.
    pub fn tensor_1d(&self, name: &str) -> Result<Vec<f64>> {
        let (tensor, data) = self.tensor(name)?;
        ensure!(tensor.shape.len() == 1, "tensor `{name}` is not 1D");
        decode_row(tensor.kind, data, tensor.shape[0])
            .with_context(|| format!("failed to decode tensor `{name}`"))
    }

    /// Decode selected logical rows from a `[width, rows]` GGUF tensor.
    pub fn tensor_rows(&self, name: &str, rows: &[usize]) -> Result<Matrix> {
        let (tensor, data) = self.tensor(name)?;
        let (width, source_rows, row_bytes) = matrix_layout(tensor, name)?;
        let mut output = Vec::with_capacity(rows.len());
        for &row in rows {
            ensure!(
                row < source_rows,
                "row {row} is outside tensor `{name}` with {source_rows} rows"
            );
            let start = row * row_bytes;
            output.push(
                decode_row(tensor.kind, &data[start..start + row_bytes], width)
                    .with_context(|| format!("failed to decode row {row} of `{name}`"))?,
            );
        }
        Ok(output)
    }

    /// Compute `input × weightᵀ`, decoding every encoded weight only once even
    /// when the input contains multiple sequence rows.
    pub fn linear_mapped(&self, name: &str, input: &[Vec<f64>]) -> Result<Matrix> {
        ensure!(!input.is_empty(), "linear input cannot be empty");
        let input_width = input[0].len();
        ensure!(
            input.iter().all(|row| row.len() == input_width),
            "linear input must be rectangular"
        );
        let (tensor, data) = self.tensor(name)?;
        let (weight_width, output_width, row_bytes) = matrix_layout(tensor, name)?;
        ensure!(
            input_width == weight_width,
            "tensor `{name}` expects {weight_width} inputs, got {input_width}"
        );

        let mut output = vec![vec![0.0; output_width]; input.len()];
        for output_col in 0..output_width {
            let start = output_col * row_bytes;
            for_each_value(
                tensor.kind,
                &data[start..start + row_bytes],
                weight_width,
                |input_col, weight| {
                    for (input_row, output_row) in input.iter().zip(output.iter_mut()) {
                        output_row[output_col] += input_row[input_col] * weight;
                    }
                },
            )
            .with_context(|| format!("failed to multiply tensor `{name}` row {output_col}"))?;
        }
        Ok(output)
    }

    /// Dot each logical row of a `[width, rows]` tensor with one vector.
    pub fn row_logits(&self, name: &str, vector: &[f64]) -> Result<Vec<f64>> {
        let (tensor, data) = self.tensor(name)?;
        let (width, rows, row_bytes) = matrix_layout(tensor, name)?;
        ensure!(
            vector.len() == width,
            "tensor `{name}` has width {width}, vector has {}",
            vector.len()
        );
        let mut logits = Vec::with_capacity(rows);
        for row in 0..rows {
            let start = row * row_bytes;
            let mut dot = 0.0;
            for_each_value(
                tensor.kind,
                &data[start..start + row_bytes],
                width,
                |column, weight| dot += weight * vector[column],
            )?;
            logits.push(dot);
        }
        Ok(logits)
    }
}

struct GgufDirectory {
    data_offset: u64,
    tensors: Vec<IndexedTensor>,
}

struct IndexedTensor {
    name: String,
    kind: u32,
    shape: Vec<usize>,
    offset: u64,
    len: usize,
}

/// Parse only the GGUF directory. Large tokenizer arrays are skipped in place,
/// so opening the model does not copy or deserialize vocabulary metadata.
fn parse_directory(file: &mut File) -> Result<GgufDirectory> {
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    ensure!(&magic == b"GGUF", "invalid GGUF magic");
    let version = read_u32(file)?;
    ensure!(
        version == 2 || version == 3,
        "unsupported GGUF version {version}"
    );
    let tensor_count = usize::try_from(read_u64(file)?).context("too many GGUF tensors")?;
    let metadata_count = usize::try_from(read_u64(file)?).context("too many GGUF metadata keys")?;

    let mut architecture = None;
    let mut alignment = 32_u64;
    for _ in 0..metadata_count {
        let key = read_string(file)?;
        let value_type = read_u32(file)?;
        match (key.as_str(), value_type) {
            ("general.architecture", 8) => architecture = Some(read_string(file)?),
            ("general.alignment", 4) => alignment = u64::from(read_u32(file)?),
            _ => skip_value(file, value_type)?,
        }
    }
    ensure!(
        architecture.as_deref() == Some("qwen2"),
        "GGUF architecture must be qwen2"
    );
    ensure!(
        alignment > 0 && alignment.is_power_of_two(),
        "invalid GGUF alignment {alignment}"
    );

    let mut tensors = Vec::with_capacity(tensor_count);
    for _ in 0..tensor_count {
        let name = read_string(file)?;
        let dimensions = usize::try_from(read_u32(file)?).context("dimension count overflow")?;
        ensure!(
            (1..=4).contains(&dimensions),
            "tensor `{name}` has invalid dimension count {dimensions}"
        );
        let shape = (0..dimensions)
            .map(|_| usize::try_from(read_u64(file)?).context("tensor dimension overflow"))
            .collect::<Result<Vec<_>>>()?;
        let kind = read_u32(file)?;
        ensure_supported(kind, &name)?;
        let offset = read_u64(file)?;
        let len = encoded_len(kind, &shape)
            .with_context(|| format!("invalid encoded size for tensor `{name}`"))?;
        tensors.push(IndexedTensor {
            name,
            kind,
            shape,
            offset,
            len,
        });
    }
    let directory_end = file.stream_position()?;
    let padding = (alignment - directory_end % alignment) % alignment;
    let data_offset = directory_end
        .checked_add(padding)
        .context("GGUF data offset overflow")?;
    Ok(GgufDirectory {
        data_offset,
        tensors,
    })
}

fn encoded_len(kind: u32, shape: &[usize]) -> Result<usize> {
    let values = shape
        .iter()
        .try_fold(1_usize, |product, &dimension| {
            product.checked_mul(dimension)
        })
        .context("tensor element count overflow")?;
    let (block_values, block_bytes) = type_layout(kind)?;
    ensure!(
        values.is_multiple_of(block_values),
        "element count {values} is not divisible by block size {block_values}"
    );
    values
        .checked_div(block_values)
        .and_then(|blocks| blocks.checked_mul(block_bytes))
        .context("tensor byte size overflow")
}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> Result<String> {
    let len = usize::try_from(read_u64(reader)?).context("GGUF string is too large")?;
    ensure!(len <= 1 << 30, "GGUF string exceeds 1 GiB");
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).context("GGUF string is not valid UTF-8")
}

fn skip_bytes(reader: &mut (impl Read + Seek), bytes: u64) -> Result<()> {
    let offset = i64::try_from(bytes).context("GGUF value is too large to seek")?;
    reader.seek(SeekFrom::Current(offset))?;
    Ok(())
}

fn skip_value(reader: &mut (impl Read + Seek), value_type: u32) -> Result<()> {
    match value_type {
        0 | 1 | 7 => skip_bytes(reader, 1),
        2 | 3 => skip_bytes(reader, 2),
        4 | 5 | 6 => skip_bytes(reader, 4),
        10 | 11 | 12 => skip_bytes(reader, 8),
        8 => {
            let len = read_u64(reader)?;
            skip_bytes(reader, len)
        }
        9 => {
            let element_type = read_u32(reader)?;
            let count = read_u64(reader)?;
            if let Some(size) = fixed_metadata_size(element_type) {
                skip_bytes(
                    reader,
                    count
                        .checked_mul(size)
                        .context("GGUF array byte size overflow")?,
                )
            } else if element_type == 8 {
                for _ in 0..count {
                    let len = read_u64(reader)?;
                    skip_bytes(reader, len)?;
                }
                Ok(())
            } else {
                bail!("unsupported GGUF array element type {element_type}")
            }
        }
        _ => Err(anyhow!("unsupported GGUF metadata type {value_type}")),
    }
}

fn fixed_metadata_size(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4 | 5 | 6 => Some(4),
        10 | 11 | 12 => Some(8),
        _ => None,
    }
}

fn ensure_supported(kind: u32, name: &str) -> Result<()> {
    match kind {
        F32 | F16 | Q5_0 | Q8_0 | Q4_K | Q6_K => Ok(()),
        _ => bail!("tensor `{name}` uses unsupported GGML type {kind}"),
    }
}

fn type_layout(kind: u32) -> Result<(usize, usize)> {
    match kind {
        F32 => Ok((1, 4)),
        F16 => Ok((1, 2)),
        Q5_0 => Ok((32, 22)),
        Q8_0 => Ok((32, 34)),
        Q4_K => Ok((256, 144)),
        Q6_K => Ok((256, 210)),
        _ => bail!("unsupported GGML type {kind}"),
    }
}

fn matrix_layout(tensor: &Tensor, name: &str) -> Result<(usize, usize, usize)> {
    ensure!(tensor.shape.len() == 2, "tensor `{name}` is not 2D");
    // GGUF/ggml stores the contiguous dimension first: [input, output].
    let width = tensor.shape[0];
    let rows = tensor.shape[1];
    let (block_values, block_bytes) = type_layout(tensor.kind)?;
    ensure!(
        width.is_multiple_of(block_values),
        "tensor `{name}` width {width} is not divisible by block size {block_values}"
    );
    let row_bytes = width / block_values * block_bytes;
    ensure!(
        row_bytes.checked_mul(rows) == Some(tensor.len),
        "tensor `{name}` byte length does not match its shape"
    );
    Ok((width, rows, row_bytes))
}

fn decode_row(kind: u32, data: &[u8], values: usize) -> Result<Vec<f64>> {
    let mut output = vec![0.0; values];
    for_each_value(kind, data, values, |index, value| output[index] = value)?;
    Ok(output)
}

fn fp16(bytes: &[u8]) -> f64 {
    f64::from(f16::from_le_bytes([bytes[0], bytes[1]]).to_f32())
}

/// Decode supported GGML blocks in their canonical logical order.
fn for_each_value(
    kind: u32,
    data: &[u8],
    values: usize,
    mut visit: impl FnMut(usize, f64),
) -> Result<()> {
    let (block_values, block_bytes) = type_layout(kind)?;
    ensure!(
        values.is_multiple_of(block_values),
        "{values} values are not divisible by block size {block_values}"
    );
    ensure!(
        data.len() == values / block_values * block_bytes,
        "encoded row has {} bytes; expected {}",
        data.len(),
        values / block_values * block_bytes
    );

    let mut base = 0;
    for block in data.chunks_exact(block_bytes) {
        match kind {
            F32 => {
                visit(
                    base,
                    f64::from(f32::from_le_bytes(block.try_into().unwrap())),
                );
            }
            F16 => visit(base, fp16(block)),
            Q8_0 => {
                let d = fp16(block);
                for (index, &quant) in block[2..34].iter().enumerate() {
                    visit(base + index, d * f64::from(quant as i8));
                }
            }
            Q5_0 => {
                let d = fp16(block);
                let high = u32::from_le_bytes(block[2..6].try_into().unwrap());
                let quants = &block[6..22];
                for (index, &packed) in quants.iter().enumerate() {
                    let high_low = (((high >> index) << 4) & 0x10) as i32;
                    let high_high = ((high >> (index + 12)) & 0x10) as i32;
                    let low = i32::from(packed & 0x0f) | high_low;
                    let high = i32::from(packed >> 4) | high_high;
                    visit(base + index, d * f64::from(low - 16));
                    visit(base + index + 16, d * f64::from(high - 16));
                }
            }
            Q4_K => {
                let d = fp16(block);
                let dmin = fp16(&block[2..]);
                let scales = &block[4..16];
                let quants = &block[16..144];
                let mut scale_index = 0;
                let mut quant_offset = 0;
                for group in 0..4 {
                    let (scale_low, min_low) = scale_min_q4(scale_index, scales);
                    let (scale_high, min_high) = scale_min_q4(scale_index + 1, scales);
                    for lane in 0..32 {
                        let packed = quants[quant_offset + lane];
                        visit(
                            base + group * 64 + lane,
                            d * f64::from(scale_low) * f64::from(packed & 0x0f)
                                - dmin * f64::from(min_low),
                        );
                        visit(
                            base + group * 64 + 32 + lane,
                            d * f64::from(scale_high) * f64::from(packed >> 4)
                                - dmin * f64::from(min_high),
                        );
                    }
                    scale_index += 2;
                    quant_offset += 32;
                }
            }
            Q6_K => {
                let low = &block[..128];
                let high = &block[128..192];
                let scales = &block[192..208];
                let d = fp16(&block[208..]);
                for half in 0..2 {
                    let low = &low[half * 64..half * 64 + 64];
                    let high = &high[half * 32..half * 32 + 32];
                    let scales = &scales[half * 8..half * 8 + 8];
                    for lane in 0..32 {
                        let scale_lane = lane / 16;
                        let q1 =
                            i32::from((low[lane] & 0x0f) | (((high[lane] >> 0) & 3) << 4)) - 32;
                        let q2 =
                            i32::from((low[lane + 32] & 0x0f) | (((high[lane] >> 2) & 3) << 4))
                                - 32;
                        let q3 = i32::from((low[lane] >> 4) | (((high[lane] >> 4) & 3) << 4)) - 32;
                        let q4 =
                            i32::from((low[lane + 32] >> 4) | (((high[lane] >> 6) & 3) << 4)) - 32;
                        let group = base + half * 128;
                        visit(
                            group + lane,
                            d * f64::from(scales[scale_lane] as i8) * f64::from(q1),
                        );
                        visit(
                            group + lane + 32,
                            d * f64::from(scales[scale_lane + 2] as i8) * f64::from(q2),
                        );
                        visit(
                            group + lane + 64,
                            d * f64::from(scales[scale_lane + 4] as i8) * f64::from(q3),
                        );
                        visit(
                            group + lane + 96,
                            d * f64::from(scales[scale_lane + 6] as i8) * f64::from(q4),
                        );
                    }
                }
            }
            _ => unreachable!("type_layout rejected the GGML type"),
        }
        base += block_values;
    }
    Ok(())
}

fn scale_min_q4(index: usize, scales: &[u8]) -> (u8, u8) {
    if index < 4 {
        (scales[index] & 63, scales[index + 4] & 63)
    } else {
        (
            (scales[index + 4] & 0x0f) | ((scales[index - 4] >> 6) << 4),
            (scales[index + 4] >> 4) | ((scales[index] >> 6) << 4),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q8_0_block_decodes_signed_values() {
        let mut block = vec![0_u8; 34];
        block[..2].copy_from_slice(&f16::from_f32(0.5).to_le_bytes());
        block[2] = 4;
        block[3] = (-6_i8) as u8;
        let values = decode_row(Q8_0, &block, 32).unwrap();
        assert_eq!(values[0], 2.0);
        assert_eq!(values[1], -3.0);
    }

    #[test]
    fn q4_k_scale_unpack_matches_ggml_layout() {
        let scales = [
            0b1100_0001,
            2,
            3,
            0b1100_0100,
            0b1000_0101,
            6,
            7,
            0b1011_1001,
            0x21,
            0x43,
            0x65,
            0x87,
        ];
        assert_eq!(scale_min_q4(0, &scales), (1, 5));
        assert_eq!(scale_min_q4(4, &scales), (0x31, 0x22));
        assert_eq!(scale_min_q4(7, &scales), (0x37, 0x28));
    }
}
