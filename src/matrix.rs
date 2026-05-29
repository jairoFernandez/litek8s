use anyhow::{Result, anyhow, ensure};
use std::thread;

pub type Matrix = Vec<Vec<f64>>;

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
