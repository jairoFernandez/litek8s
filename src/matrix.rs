use anyhow::{Context, Result, anyhow, bail, ensure};
use std::path::Path;
use std::thread;

pub type Matrix = Vec<Vec<f64>>;

/// Magic prefix for the on-disk weight format, so a truncated or foreign file
/// is rejected rather than read as garbage dimensions.
const MATRIX_BIN_MAGIC: &[u8; 4] = b"LK8M";

pub fn multiply_sequential(left: &[Vec<f64>], right: &[Vec<f64>]) -> Result<Matrix> {
    let (rows, cols, shared) = validate_dimensions(left, right)?;
    let right_t = transpose(right, shared, cols);
    let mut result = vec![vec![0.0; cols]; rows];

    for (row_idx, row) in left.iter().enumerate() {
        for (col_idx, col) in right_t.iter().enumerate() {
            result[row_idx][col_idx] = dot(row, col);
        }
    }

    Ok(result)
}

pub fn multiply_parallel(left: &[Vec<f64>], right: &[Vec<f64>]) -> Result<Matrix> {
    let (rows, cols, shared) = validate_dimensions(left, right)?;
    let right_t = transpose(right, shared, cols);
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(rows.max(1));
    let chunk_size = rows.div_ceil(workers);

    let chunks = thread::scope(|scope| {
        let mut handles = Vec::new();

        for start in (0..rows).step_by(chunk_size) {
            let end = (start + chunk_size).min(rows);
            let left_chunk = &left[start..end];
            let right_t = &right_t;

            handles.push(scope.spawn(move || {
                let mut partial = Vec::with_capacity(left_chunk.len());
                for row in left_chunk {
                    let mut out_row = Vec::with_capacity(cols);
                    for col in right_t {
                        out_row.push(dot(row, col));
                    }
                    partial.push(out_row);
                }
                partial
            }));
        }

        let mut all_chunks = Vec::with_capacity(handles.len());
        for handle in handles {
            all_chunks.push(handle.join().map_err(|_| anyhow!("matrix worker panicked"))?);
        }
        Ok::<Vec<Matrix>, anyhow::Error>(all_chunks)
    })?;

    Ok(chunks.into_iter().flatten().collect())
}

/// Multiply a horizontal band of `left` (a contiguous set of rows) against the
/// full `right` matrix. This is the unit of work shipped to a remote node: each
/// node owns a row band, computes it with its own cores via `multiply_parallel`,
/// and returns the resulting rows. Stitching the bands back in order reproduces
/// the full `left · right` product — the same decomposition tensor/model
/// parallelism uses to shard a weight matmul across machines.
pub fn multiply_band(left_band: &[Vec<f64>], right: &[Vec<f64>]) -> Result<Matrix> {
    if left_band.is_empty() {
        // An empty band is legal (a node may be handed zero rows when the row
        // count is smaller than the node count) — nothing to multiply.
        ensure!(!right.is_empty(), "right matrix cannot be empty");
        return Ok(Vec::new());
    }
    multiply_parallel(left_band, right)
}

/// Multiply `left` (the activation, replicated to every node) against a vertical
/// band of the weight matrix — i.e. a contiguous subset of `right`'s columns,
/// already stored column-major as `right_cols` where each inner Vec is one
/// column. Returns `left.rows x right_cols.len()`: this node's slice of the
/// layer output. Concatenating every node's slice horizontally reproduces the
/// full `left · right`. This is the column (model-parallel) shard: each node
/// holds only its columns of the weight, so a weight too large for one node is
/// split across many.
pub fn multiply_column_shard(left: &[Vec<f64>], right_cols: &[Vec<f64>]) -> Result<Matrix> {
    ensure!(!left.is_empty(), "activation cannot be empty");
    let shared = left[0].len();
    ensure!(
        left.iter().all(|row| row.len() == shared),
        "activation must be rectangular"
    );
    if right_cols.is_empty() {
        // A node may own zero columns when there are more nodes than columns.
        return Ok(left.iter().map(|_| Vec::new()).collect());
    }
    ensure!(
        right_cols.iter().all(|col| col.len() == shared),
        "weight column length ({shared}) must match activation width"
    );

    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(left.len().max(1));
    let chunk_size = left.len().div_ceil(workers);

    let chunks = thread::scope(|scope| {
        let mut handles = Vec::new();
        for start in (0..left.len()).step_by(chunk_size) {
            let end = (start + chunk_size).min(left.len());
            let rows = &left[start..end];
            handles.push(scope.spawn(move || {
                rows.iter()
                    .map(|row| right_cols.iter().map(|col| dot(row, col)).collect())
                    .collect::<Matrix>()
            }));
        }
        let mut all = Vec::with_capacity(handles.len());
        for handle in handles {
            all.push(handle.join().map_err(|_| anyhow!("matrix worker panicked"))?);
        }
        Ok::<Vec<Matrix>, anyhow::Error>(all)
    })?;

    Ok(chunks.into_iter().flatten().collect())
}

/// Transpose a matrix into its columns: `columns(m)[j][i] == m[i][j]`. Weights
/// are sharded and cached column-major so a node stores exactly the columns it
/// owns, contiguous on disk.
pub fn columns(matrix: &[Vec<f64>]) -> Matrix {
    if matrix.is_empty() {
        return Vec::new();
    }
    let cols = matrix[0].len();
    let rows = matrix.len();
    transpose(matrix, rows, cols)
}

/// Apply ReLU in place: clamp negatives to zero. Cheap non-linearity so the
/// pipeline behaves like a real feed-forward network rather than collapsing
/// into a single linear map across layers.
pub fn apply_relu(matrix: &mut [Vec<f64>]) {
    for row in matrix {
        for value in row {
            if *value < 0.0 {
                *value = 0.0;
            }
        }
    }
}

/// Split `total` columns across nodes proportional to their integer weights
/// (e.g. CPU cores), returning the column count for each node. Every weight is
/// treated as at least 1 so no node is starved; the largest share absorbs
/// rounding so the parts always sum to `total`.
pub fn weighted_split(total: usize, weights: &[u32]) -> Vec<usize> {
    if weights.is_empty() {
        return Vec::new();
    }
    let adjusted: Vec<u64> = weights.iter().map(|&w| u64::from(w.max(1))).collect();
    let sum: u64 = adjusted.iter().sum();
    let mut parts: Vec<usize> = adjusted
        .iter()
        .map(|&w| (w * total as u64 / sum) as usize)
        .collect();
    // Hand any rounding remainder to the heaviest node.
    let assigned: usize = parts.iter().sum();
    if assigned < total {
        let heaviest = adjusted
            .iter()
            .enumerate()
            .max_by_key(|&(_, &w)| w)
            .map(|(i, _)| i)
            .unwrap_or(0);
        parts[heaviest] += total - assigned;
    }
    parts
}

/// Build a deterministic `rows x cols` matrix from a seed. Lets the demo stress
/// the cluster with an arbitrarily large input (`--size N`) while keeping the
/// result reproducible across runs and nodes — no RNG dependency, no I/O.
pub fn deterministic_matrix(rows: usize, cols: usize, seed: u64) -> Matrix {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        // SplitMix64: cheap, well-distributed, fully reproducible.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Map to a small signed-ish float so products stay readable.
        (z >> 40) as f64 / 16_777_216.0 - 0.5
    };
    (0..rows)
        .map(|_| (0..cols).map(|_| next()).collect())
        .collect()
}

/// Content-address a matrix: a stable hex digest of its dimensions and values.
/// Two identical matrices get the same id, so a weight matrix is uploaded to a
/// node at most once regardless of how many matmuls reference it.
pub fn matrix_id(matrix: &[Vec<f64>]) -> String {
    // FNV-1a over the same byte layout used on disk — cheap, no extra deps, and
    // collisions are astronomically unlikely for the distinct weight sets a
    // home cluster will hold.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    let rows = matrix.len() as u64;
    let cols = matrix.first().map(|r| r.len()).unwrap_or(0) as u64;
    feed(&rows.to_le_bytes());
    feed(&cols.to_le_bytes());
    for row in matrix {
        for value in row {
            feed(&value.to_le_bytes());
        }
    }
    format!("{hash:016x}")
}

/// Serialize a matrix to the compact binary format: magic + rows/cols (u64 LE)
/// + row-major f64 LE values. Far cheaper than JSON for the large weight
/// matrices the cache holds (8 bytes/f64 vs ~18 chars as JSON text).
pub fn encode_matrix_bin(matrix: &[Vec<f64>]) -> Result<Vec<u8>> {
    let rows = matrix.len();
    let cols = matrix.first().map(|r| r.len()).unwrap_or(0);
    ensure!(
        matrix.iter().all(|row| row.len() == cols),
        "cannot serialize a ragged matrix"
    );

    let mut buf = Vec::with_capacity(20 + rows * cols * 8);
    buf.extend_from_slice(MATRIX_BIN_MAGIC);
    buf.extend_from_slice(&(rows as u64).to_le_bytes());
    buf.extend_from_slice(&(cols as u64).to_le_bytes());
    for row in matrix {
        for value in row {
            buf.extend_from_slice(&value.to_le_bytes());
        }
    }
    Ok(buf)
}

/// Encode and write a matrix to disk atomically.
pub fn write_matrix_bin(path: &Path, matrix: &[Vec<f64>]) -> Result<()> {
    let buf = encode_matrix_bin(matrix)?;
    write_bytes_atomic(path, &buf)
}

/// Write an already-encoded bin blob to disk, validating its header and that the
/// declared dimensions match the payload length so a corrupt or truncated upload
/// is rejected before it lands. Used on the agent side where the blob arrives
/// raw over the wire.
pub fn write_bin_blob(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() >= 20, "weight blob is truncated");
    ensure!(
        &bytes[0..4] == MATRIX_BIN_MAGIC,
        "weight blob has an unrecognized format"
    );
    let rows = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
    let cols = u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize;
    ensure!(
        bytes.len() == 20 + rows * cols * 8,
        "weight blob length {} does not match {rows}x{cols}",
        bytes.len()
    );
    write_bytes_atomic(path, bytes)
}

/// Write bytes to `path` atomically: stage to a sibling file then rename, so a
/// reader never sees a partial file if two uploads of the same id race.
fn write_bytes_atomic(path: &Path, buf: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create weights dir {}", parent.display()))?;
    }
    let staging = path.with_extension("partial");
    std::fs::write(&staging, buf)
        .with_context(|| format!("failed to write weight file {}", staging.display()))?;
    std::fs::rename(&staging, path)
        .with_context(|| format!("failed to finalize weight file {}", path.display()))?;
    Ok(())
}

/// Read a matrix written by [`write_matrix_bin`], validating the magic and that
/// the declared dimensions match the payload length.
pub fn read_matrix_bin(path: &Path) -> Result<Matrix> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read weight file {}", path.display()))?;
    ensure!(bytes.len() >= 20, "weight file {} is truncated", path.display());
    ensure!(
        &bytes[0..4] == MATRIX_BIN_MAGIC,
        "weight file {} has an unrecognized format",
        path.display()
    );

    let rows = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
    let cols = u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize;
    let expected = 20 + rows * cols * 8;
    ensure!(
        bytes.len() == expected,
        "weight file {} length {} does not match {rows}x{cols}",
        path.display(),
        bytes.len()
    );

    let mut matrix = Vec::with_capacity(rows);
    let mut offset = 20;
    for _ in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for _ in 0..cols {
            let value = f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
            row.push(value);
            offset += 8;
        }
        matrix.push(row);
    }
    Ok(matrix)
}

/// Validate that a weight id is a safe filename component (the agent turns it
/// into a path). Hex digests from [`matrix_id`] always pass; this guards against
/// a caller injecting `../` or absolute paths.
pub fn validate_weight_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty(), "weight id must not be empty");
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        bail!("weight id `{id}` contains unsupported characters");
    }
    Ok(())
}

fn validate_dimensions(left: &[Vec<f64>], right: &[Vec<f64>]) -> Result<(usize, usize, usize)> {
    ensure!(!left.is_empty(), "left matrix cannot be empty");
    ensure!(!right.is_empty(), "right matrix cannot be empty");

    let shared = left[0].len();
    let cols = right[0].len();

    ensure!(shared > 0, "left matrix must have at least one column");
    ensure!(cols > 0, "right matrix must have at least one column");

    ensure!(
        left.iter().all(|row| row.len() == shared),
        "left matrix must be rectangular"
    );
    ensure!(
        right.iter().all(|row| row.len() == cols),
        "right matrix must be rectangular"
    );
    ensure!(
        right.len() == shared,
        "dimension mismatch: left columns ({shared}) must match right rows ({})",
        right.len()
    );

    Ok((left.len(), cols, shared))
}

fn transpose(matrix: &[Vec<f64>], rows: usize, cols: usize) -> Matrix {
    let mut transposed = vec![vec![0.0; rows]; cols];
    for (row_idx, row) in matrix.iter().enumerate() {
        for (col_idx, value) in row.iter().enumerate() {
            transposed[col_idx][row_idx] = *value;
        }
    }
    transposed
}

fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter()
        .zip(right.iter())
        .map(|(lhs, rhs)| lhs * rhs)
        .sum()
}
