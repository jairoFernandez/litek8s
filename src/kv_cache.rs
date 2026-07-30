//! File-backed KV cache for local incremental inference.
//!
//! Each transformer layer owns one grow-on-demand file. Layers are distributed
//! round-robin across the configured directories, so separate disks or volumes
//! can share capacity. K/V rows are stored as F16 and accessed through `mmap`;
//! attention only decodes a configurable tile into RAM.

use std::fs::{File, OpenOptions, create_dir_all, remove_file};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use half::f16;
use memmap2::{MmapMut, MmapOptions};

use crate::transformer::apply_rope;

const HEADER_BYTES: usize = 64;
const MAGIC: &[u8; 8] = b"L8KVC001";
const VERSION: u32 = 1;
const DTYPE_F16: u32 = 1;
const GROW_TOKENS: usize = 64;

#[derive(Debug, Clone)]
pub struct DiskKvConfig {
    pub directories: Vec<PathBuf>,
    pub num_layers: usize,
    pub num_kv_heads: usize,
    pub head_dim: usize,
    pub max_tokens: usize,
    pub buffer_bytes: usize,
    pub keep_files: bool,
}

#[derive(Debug, Clone)]
pub struct DiskKvStats {
    pub files: usize,
    pub directories: usize,
    pub tokens: usize,
    pub used_bytes: u64,
    pub mapped_bytes: u64,
    pub logical_capacity_bytes: u64,
    pub buffer_bytes: usize,
    pub paths: Vec<PathBuf>,
}

pub struct DiskKvCache {
    layers: Vec<LayerCache>,
    num_kv_heads: usize,
    head_dim: usize,
    max_tokens: usize,
    buffer_bytes: usize,
    scratch: Vec<f32>,
    scores: Vec<f64>,
    keep_files: bool,
    directory_count: usize,
}

struct LayerCache {
    path: PathBuf,
    file: File,
    mmap: Option<MmapMut>,
    layer: usize,
    kv_width: usize,
    max_tokens: usize,
    row_bytes: usize,
    mapped_tokens: usize,
    mapped_bytes: usize,
    len: usize,
}

#[derive(Clone, Copy)]
enum Component {
    Key,
    Value,
}

impl DiskKvCache {
    pub fn create(config: DiskKvConfig) -> Result<Self> {
        ensure!(
            !config.directories.is_empty(),
            "at least one KV cache directory is required"
        );
        ensure!(config.num_layers > 0, "KV cache needs at least one layer");
        ensure!(
            config.num_kv_heads > 0 && config.head_dim > 0,
            "invalid KV dimensions"
        );
        ensure!(config.max_tokens > 0, "KV max tokens must be positive");
        ensure!(config.buffer_bytes > 0, "KV buffer must be positive");

        for directory in &config.directories {
            create_dir_all(directory).with_context(|| {
                format!(
                    "failed to create KV cache directory {}",
                    directory.display()
                )
            })?;
        }

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        let session = format!("{}-{stamp}", std::process::id());
        let kv_width = config
            .num_kv_heads
            .checked_mul(config.head_dim)
            .context("KV width overflow")?;
        let mut layers = Vec::with_capacity(config.num_layers);
        for layer in 0..config.num_layers {
            let directory = &config.directories[layer % config.directories.len()];
            let path = directory.join(format!("litek8s-kv-{session}-layer-{layer:03}.bin"));
            layers.push(LayerCache::create(
                &path,
                layer,
                kv_width,
                config.max_tokens,
            )?);
        }

        Ok(Self {
            layers,
            num_kv_heads: config.num_kv_heads,
            head_dim: config.head_dim,
            max_tokens: config.max_tokens,
            buffer_bytes: config.buffer_bytes,
            scratch: Vec::new(),
            scores: Vec::new(),
            keep_files: config.keep_files,
            directory_count: config.directories.len(),
        })
    }

    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    pub fn tokens(&self) -> usize {
        self.layers.first().map_or(0, |layer| layer.len)
    }

    pub fn stats(&self) -> DiskKvStats {
        let tokens = self.tokens();
        DiskKvStats {
            files: self.layers.len(),
            directories: self.directory_count,
            tokens,
            used_bytes: self
                .layers
                .iter()
                .map(|layer| (HEADER_BYTES + layer.len * layer.row_bytes) as u64)
                .sum(),
            mapped_bytes: self
                .layers
                .iter()
                .map(|layer| layer.mapped_bytes as u64)
                .sum(),
            logical_capacity_bytes: self
                .layers
                .iter()
                .map(|layer| (HEADER_BYTES + layer.max_tokens * layer.row_bytes) as u64)
                .sum(),
            buffer_bytes: self.buffer_bytes,
            paths: self.layers.iter().map(|layer| layer.path.clone()).collect(),
        }
    }

    /// Append one layer's rotated K and V row, then attend over all rows stored
    /// for that layer. Only a tile controlled by `buffer_bytes` is decoded.
    #[allow(clippy::too_many_arguments)]
    pub fn attention_step(
        &mut self,
        layer_index: usize,
        q_new: &[f64],
        k_new: &[f64],
        v_new: &[f64],
        n_q_heads: usize,
        pos: usize,
        cos: &[f64],
        sin: &[f64],
    ) -> Result<Vec<f64>> {
        ensure!(
            layer_index < self.layers.len(),
            "KV layer {layer_index} is out of range"
        );
        ensure!(
            n_q_heads.is_multiple_of(self.num_kv_heads),
            "query heads must be divisible by KV heads"
        );
        let kv_width = self.num_kv_heads * self.head_dim;
        ensure!(
            q_new.len() == n_q_heads * self.head_dim,
            "query width mismatch"
        );
        ensure!(
            k_new.len() == kv_width && v_new.len() == kv_width,
            "key/value width mismatch"
        );
        ensure!(pos < self.max_tokens, "KV position exceeds cache capacity");

        let mut key = k_new.to_vec();
        for kv_head in 0..self.num_kv_heads {
            let start = kv_head * self.head_dim;
            apply_rope(&mut key[start..start + self.head_dim], cos, sin);
        }
        let mut query = q_new.to_vec();
        for query_head in 0..n_q_heads {
            let start = query_head * self.head_dim;
            apply_rope(&mut query[start..start + self.head_dim], cos, sin);
        }

        let Self {
            layers,
            scratch,
            scores,
            buffer_bytes,
            num_kv_heads,
            head_dim,
            ..
        } = self;
        let layer = &mut layers[layer_index];
        layer.append(pos, &key, v_new)?;
        let sequence = layer.len;
        let group = n_q_heads / *num_kv_heads;
        let scale = 1.0 / (*head_dim as f64).sqrt();
        let tile_tokens = (*buffer_bytes / (*head_dim * size_of::<f32>()))
            .max(1)
            .min(sequence);
        let mut output = vec![0.0; n_q_heads * *head_dim];

        for kv_head in 0..*num_kv_heads {
            scores.resize(group * sequence, 0.0);
            scores.fill(0.0);

            for tile_start in (0..sequence).step_by(tile_tokens) {
                let count = tile_tokens.min(sequence - tile_start);
                layer.load_head(
                    Component::Key,
                    kv_head,
                    *head_dim,
                    tile_start,
                    count,
                    scratch,
                );
                for local in 0..count {
                    let position = tile_start + local;
                    let cached = &scratch[local * *head_dim..(local + 1) * *head_dim];
                    for query_in_group in 0..group {
                        let query_head = kv_head * group + query_in_group;
                        let query_start = query_head * *head_dim;
                        let dot = query[query_start..query_start + *head_dim]
                            .iter()
                            .zip(cached)
                            .map(|(left, right)| left * f64::from(*right))
                            .sum::<f64>();
                        scores[query_in_group * sequence + position] = dot * scale;
                    }
                }
            }
            for query_in_group in 0..group {
                softmax_in_place(
                    &mut scores[query_in_group * sequence..(query_in_group + 1) * sequence],
                );
            }

            for tile_start in (0..sequence).step_by(tile_tokens) {
                let count = tile_tokens.min(sequence - tile_start);
                layer.load_head(
                    Component::Value,
                    kv_head,
                    *head_dim,
                    tile_start,
                    count,
                    scratch,
                );
                for local in 0..count {
                    let position = tile_start + local;
                    let cached = &scratch[local * *head_dim..(local + 1) * *head_dim];
                    for query_in_group in 0..group {
                        let query_head = kv_head * group + query_in_group;
                        let output_start = query_head * *head_dim;
                        let weight = scores[query_in_group * sequence + position];
                        for dimension in 0..*head_dim {
                            output[output_start + dimension] +=
                                weight * f64::from(cached[dimension]);
                        }
                    }
                }
            }
        }
        Ok(output)
    }
}

impl Drop for DiskKvCache {
    fn drop(&mut self) {
        for layer in &mut self.layers {
            if let Some(mmap) = layer.mmap.take() {
                let _ = mmap.flush_async();
                drop(mmap);
            }
        }
        if !self.keep_files {
            for layer in &self.layers {
                let _ = remove_file(&layer.path);
            }
        }
    }
}

impl LayerCache {
    fn create(path: &Path, layer: usize, kv_width: usize, max_tokens: usize) -> Result<Self> {
        let row_bytes = kv_width
            .checked_mul(2)
            .and_then(|value| value.checked_mul(size_of::<u16>()))
            .context("KV row byte size overflow")?;
        HEADER_BYTES
            .checked_add(
                max_tokens
                    .checked_mul(row_bytes)
                    .context("KV file size overflow")?,
            )
            .context("KV file size overflow")?;

        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .with_context(|| format!("failed to create KV cache {}", path.display()))?;
        file.set_len(HEADER_BYTES as u64)
            .with_context(|| format!("failed to resize KV cache {}", path.display()))?;
        // SAFETY: this process exclusively owns the newly created cache file.
        let mut mmap = unsafe { MmapOptions::new().len(HEADER_BYTES).map_mut(&file) }
            .with_context(|| format!("failed to mmap KV cache {}", path.display()))?;
        write_header(&mut mmap, layer, kv_width, max_tokens, row_bytes, 0);
        mmap.flush_range(0, HEADER_BYTES)
            .context("failed to persist KV header")?;
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential)
            .context("failed to set sequential KV mmap advice")?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            mmap: Some(mmap),
            layer,
            kv_width,
            max_tokens,
            row_bytes,
            mapped_tokens: 0,
            mapped_bytes: HEADER_BYTES,
            len: 0,
        })
    }

    fn append(&mut self, pos: usize, key: &[f64], value: &[f64]) -> Result<()> {
        ensure!(
            pos == self.len,
            "KV layer {} expected position {}, got {pos}",
            self.layer,
            self.len
        );
        ensure!(pos < self.max_tokens, "KV cache capacity exceeded");
        ensure!(
            key.len() == self.kv_width && value.len() == self.kv_width,
            "KV row width mismatch"
        );
        self.grow_for(pos)?;
        let row_start = HEADER_BYTES + pos * self.row_bytes;
        let mmap = self.mmap.as_mut().context("KV mmap is closed")?;
        let mut offset = row_start;
        for &number in key.iter().chain(value) {
            let encoded = f16::from_f64(number).to_le_bytes();
            mmap[offset..offset + 2].copy_from_slice(&encoded);
            offset += 2;
        }
        self.len += 1;
        mmap[32..40].copy_from_slice(&(self.len as u64).to_le_bytes());
        mmap.flush_async_range(row_start, self.row_bytes)
            .context("failed to queue KV row flush")?;
        Ok(())
    }

    /// Extend and remap in small token groups. Mapping the full configured
    /// capacity up front causes APFS to allocate every block even when only a
    /// few rows are written, defeating the disk-saving purpose of the cache.
    fn grow_for(&mut self, pos: usize) -> Result<()> {
        if pos < self.mapped_tokens {
            return Ok(());
        }
        let mapped_tokens = pos
            .checked_add(GROW_TOKENS)
            .context("KV mapped token count overflow")?
            / GROW_TOKENS
            * GROW_TOKENS;
        let mapped_tokens = mapped_tokens.min(self.max_tokens);
        let mapped_bytes = HEADER_BYTES
            .checked_add(
                mapped_tokens
                    .checked_mul(self.row_bytes)
                    .context("KV mapped byte size overflow")?,
            )
            .context("KV mapped byte size overflow")?;

        if let Some(mmap) = self.mmap.take() {
            mmap.flush()
                .with_context(|| format!("failed to flush KV cache {}", self.path.display()))?;
            drop(mmap);
        }
        self.file
            .set_len(u64::try_from(mapped_bytes).context("KV cache is too large")?)
            .with_context(|| format!("failed to grow KV cache {}", self.path.display()))?;
        // SAFETY: this cache exclusively owns the file and remaps only after
        // dropping its previous mapping.
        let mmap = unsafe { MmapOptions::new().len(mapped_bytes).map_mut(&self.file) }
            .with_context(|| format!("failed to remap KV cache {}", self.path.display()))?;
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential)
            .context("failed to set sequential KV mmap advice")?;
        self.mmap = Some(mmap);
        self.mapped_tokens = mapped_tokens;
        self.mapped_bytes = mapped_bytes;
        Ok(())
    }

    fn load_head(
        &self,
        component: Component,
        kv_head: usize,
        head_dim: usize,
        start_token: usize,
        count: usize,
        output: &mut Vec<f32>,
    ) {
        output.resize(count * head_dim, 0.0);
        let component_values = match component {
            Component::Key => 0,
            Component::Value => self.kv_width,
        };
        let head_values = component_values + kv_head * head_dim;
        let mmap = self.mmap.as_ref().expect("KV mmap remains open");
        for local in 0..count {
            let token = start_token + local;
            let row_start = HEADER_BYTES + token * self.row_bytes;
            let head_start = row_start + head_values * 2;
            for dimension in 0..head_dim {
                let offset = head_start + dimension * 2;
                output[local * head_dim + dimension] =
                    f16::from_le_bytes([mmap[offset], mmap[offset + 1]]).to_f32();
            }
        }
    }
}

fn write_header(
    mmap: &mut MmapMut,
    layer: usize,
    kv_width: usize,
    max_tokens: usize,
    row_bytes: usize,
    len: usize,
) {
    mmap[..HEADER_BYTES].fill(0);
    mmap[..8].copy_from_slice(MAGIC);
    mmap[8..12].copy_from_slice(&VERSION.to_le_bytes());
    mmap[12..16].copy_from_slice(&(layer as u32).to_le_bytes());
    mmap[16..24].copy_from_slice(&(max_tokens as u64).to_le_bytes());
    mmap[24..28].copy_from_slice(&(kv_width as u32).to_le_bytes());
    mmap[28..32].copy_from_slice(&DTYPE_F16.to_le_bytes());
    mmap[32..40].copy_from_slice(&(len as u64).to_le_bytes());
    mmap[40..48].copy_from_slice(&(row_bytes as u64).to_le_bytes());
}

fn softmax_in_place(values: &mut [f64]) {
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut sum = 0.0;
    for value in values.iter_mut() {
        *value = (*value - maximum).exp();
        sum += *value;
    }
    if sum != 0.0 {
        for value in values {
            *value /= sum;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transformer::{KvCache, gqa_step};

    fn config(directories: Vec<PathBuf>) -> DiskKvConfig {
        DiskKvConfig {
            directories,
            num_layers: 2,
            num_kv_heads: 1,
            head_dim: 4,
            max_tokens: 8,
            buffer_bytes: 32,
            keep_files: false,
        }
    }

    #[test]
    fn disk_attention_tracks_the_in_memory_reference() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut disk =
            DiskKvCache::create(config(vec![first.path().into(), second.path().into()])).unwrap();
        let paths = disk.stats().paths;
        assert!(paths[0].starts_with(first.path()));
        assert!(paths[1].starts_with(second.path()));

        let mut memory = KvCache::default();
        for pos in 0..3 {
            let q = vec![0.2 + pos as f64, -0.4, 0.7, 0.1];
            let k = vec![0.3, 0.2 + pos as f64, -0.5, 0.8];
            let v = vec![0.1 * pos as f64, 0.6, -0.2, 0.9];
            let cos = [1.0, 1.0];
            let sin = [0.0, 0.0];
            let expected = gqa_step(&q, &k, &v, &mut memory, 1, 1, 4, &cos, &sin);
            let actual = disk
                .attention_step(0, &q, &k, &v, 1, pos, &cos, &sin)
                .unwrap();
            for (left, right) in actual.iter().zip(expected) {
                assert!((left - right).abs() < 5e-4, "{left} != {right}");
            }
        }
        assert_eq!(disk.tokens(), 3);
        drop(disk);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn rejects_positions_past_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(vec![directory.path().into()]);
        config.num_layers = 1;
        config.max_tokens = 1;
        let mut cache = DiskKvCache::create(config).unwrap();
        let vector = [0.0; 4];
        let result =
            cache.attention_step(0, &vector, &vector, &vector, 1, 1, &[1.0, 1.0], &[0.0, 0.0]);
        assert!(result.is_err());
    }

    #[test]
    fn grows_files_in_small_groups_instead_of_allocating_maximum() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(vec![directory.path().into()]);
        config.num_layers = 1;
        config.max_tokens = 1024;
        let mut cache = DiskKvCache::create(config).unwrap();
        let vector = [0.0; 4];

        for position in 0..65 {
            cache
                .attention_step(
                    0,
                    &vector,
                    &vector,
                    &vector,
                    1,
                    position,
                    &[1.0, 1.0],
                    &[0.0, 0.0],
                )
                .unwrap();
        }

        let stats = cache.stats();
        assert_eq!(stats.tokens, 65);
        assert_eq!(stats.mapped_bytes as usize, HEADER_BYTES + 128 * 16);
        assert!(stats.mapped_bytes < stats.logical_capacity_bytes);
    }
}
