use litek8s::matrix::{
    apply_relu, columns, deterministic_matrix, encode_matrix_bin, encode_matrix_bin_dtype,
    matrix_id, multiply_band, multiply_column_shard, multiply_parallel, multiply_sequential,
    read_matrix_bin, validate_weight_id, weighted_split, write_bin_blob, write_matrix_bin, BinDtype,
};

#[test]
fn parallel_matches_sequential_result() {
    let left = vec![
        vec![1.0, 2.0, 3.0],
        vec![4.0, 5.0, 6.0],
        vec![7.0, 8.0, 9.0],
        vec![10.0, 11.0, 12.0],
    ];
    let right = vec![
        vec![2.0, 0.0],
        vec![1.0, 2.0],
        vec![3.0, 1.0],
    ];

    let sequential = multiply_sequential(&left, &right).expect("sequential result");
    let parallel = multiply_parallel(&left, &right).expect("parallel result");

    assert_eq!(parallel, sequential);
    assert_eq!(
        parallel,
        vec![
            vec![13.0, 7.0],
            vec![31.0, 16.0],
            vec![49.0, 25.0],
            vec![67.0, 34.0],
        ]
    );
}

#[test]
fn rejects_dimension_mismatch() {
    let left = vec![vec![1.0, 2.0]];
    let right = vec![vec![1.0, 2.0]];

    let error = multiply_parallel(&left, &right).expect_err("must reject invalid shapes");
    assert!(error.to_string().contains("dimension mismatch"));
}

#[test]
fn row_bands_stitch_back_to_full_product() {
    // Splitting `left` into row bands, multiplying each band against `right`,
    // and concatenating the results must reproduce the full product — the
    // invariant the distributed matmul relies on.
    let left = deterministic_matrix(9, 6, 42);
    let right = deterministic_matrix(6, 7, 99);
    let full = multiply_parallel(&left, &right).expect("full product");

    let mut stitched = Vec::new();
    for band in left.chunks(4) {
        stitched.extend(multiply_band(band, &right).expect("band product"));
    }

    assert_eq!(stitched, full);
}

#[test]
fn empty_band_yields_no_rows() {
    let right = deterministic_matrix(3, 4, 7);
    let band: Vec<Vec<f64>> = Vec::new();
    assert!(multiply_band(&band, &right).expect("empty band ok").is_empty());
}

#[test]
fn deterministic_matrix_is_reproducible() {
    // Same seed must produce identical matrices so every node agrees on input.
    assert_eq!(
        deterministic_matrix(5, 5, 123),
        deterministic_matrix(5, 5, 123)
    );
    assert_ne!(
        deterministic_matrix(5, 5, 123),
        deterministic_matrix(5, 5, 124)
    );
}

#[test]
fn matrix_bin_roundtrips_through_disk() {
    let matrix = deterministic_matrix(17, 23, 5);
    let dir = std::env::temp_dir().join(format!("litek8s-test-weights-{}", std::process::id()));
    let path = dir.join("w.bin");
    write_matrix_bin(&path, &matrix).expect("write");
    let read = read_matrix_bin(&path).expect("read");
    assert_eq!(read, matrix);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn encoded_blob_writes_and_reads_back() {
    // The blob produced by encode_matrix_bin (shipped raw over the wire) must
    // land via write_bin_blob and read back identical.
    let matrix = deterministic_matrix(11, 9, 3);
    let blob = encode_matrix_bin(&matrix).unwrap();
    let dir = std::env::temp_dir().join(format!("litek8s-blob-test-{}", std::process::id()));
    let path = dir.join("w.bin");
    write_bin_blob(&path, &blob).unwrap();
    assert_eq!(read_matrix_bin(&path).unwrap(), matrix);
    // A truncated or foreign blob is rejected, not written as garbage.
    assert!(write_bin_blob(&path, b"too short").is_err());
    assert!(write_bin_blob(&path, &blob[..blob.len() - 8]).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn f16_and_bf16_storage_roundtrip_within_tolerance() {
    // Storing at 16-bit and reading back loses precision but stays close, and
    // the byte size is a quarter of f64.
    let matrix = deterministic_matrix(6, 5, 2);
    let dir = std::env::temp_dir().join(format!("litek8s-f16-test-{}", std::process::id()));
    for (dtype, tol, per_elem) in [
        (BinDtype::F16, 1e-2, 2),
        (BinDtype::Bf16, 5e-2, 2),
        (BinDtype::F32, 1e-6, 4),
    ] {
        let blob = encode_matrix_bin_dtype(&matrix, dtype).unwrap();
        // header(22) + rows*cols*per_elem
        assert_eq!(blob.len(), 22 + 6 * 5 * per_elem);
        let path = dir.join(format!("{per_elem}.bin"));
        write_bin_blob(&path, &blob).unwrap();
        let back = read_matrix_bin(&path).unwrap();
        for (r0, r1) in matrix.iter().zip(back.iter()) {
            for (a, b) in r0.iter().zip(r1.iter()) {
                assert!((a - b).abs() <= tol, "{dtype:?}: {a} vs {b}");
            }
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn matrix_id_is_content_addressed() {
    // Same content → same id; any change → different id.
    let a = deterministic_matrix(8, 8, 1);
    let b = deterministic_matrix(8, 8, 1);
    let c = deterministic_matrix(8, 8, 2);
    assert_eq!(matrix_id(&a), matrix_id(&b));
    assert_ne!(matrix_id(&a), matrix_id(&c));
    // Hex digest is always a safe filename component.
    assert!(validate_weight_id(&matrix_id(&a)).is_ok());
}

#[test]
fn rejects_unsafe_weight_ids() {
    assert!(validate_weight_id("../escape").is_err());
    assert!(validate_weight_id("/abs/path").is_err());
    assert!(validate_weight_id("").is_err());
    assert!(validate_weight_id("abc123_-").is_ok());
}

#[test]
fn column_shards_concatenate_to_full_product() {
    // Splitting the weight's columns across shards, computing each shard, and
    // concatenating horizontally must equal the full activation·weight — the
    // invariant the layer pipeline relies on.
    let act = deterministic_matrix(5, 6, 1);
    let weight = deterministic_matrix(6, 9, 2);
    let full = multiply_parallel(&act, &weight).expect("full product");

    let cols = columns(&weight);
    let splits = weighted_split(9, &[1, 2, 3]); // 3 nodes, uneven weights
    assert_eq!(splits.iter().sum::<usize>(), 9);

    let mut rebuilt = vec![Vec::new(); act.len()];
    let mut cursor = 0;
    for n in splits {
        let shard = &cols[cursor..cursor + n];
        cursor += n;
        let out = multiply_column_shard(&act, shard).expect("shard");
        for (i, row) in out.into_iter().enumerate() {
            rebuilt[i].extend(row);
        }
    }
    assert_eq!(rebuilt, full);
}

#[test]
fn sharded_pipeline_matches_local_forward() {
    // A 2-layer ReLU pipeline computed via column shards (the distributed path)
    // must equal the same forward done locally and undivided.
    let input = deterministic_matrix(3, 8, 1);
    let w0 = deterministic_matrix(8, 10, 2);
    let w1 = deterministic_matrix(10, 6, 3);

    // Reference: plain local forward with ReLU between layers.
    let mut a = multiply_parallel(&input, &w0).unwrap();
    apply_relu(&mut a);
    let mut reference = multiply_parallel(&a, &w1).unwrap();
    apply_relu(&mut reference);

    // Sharded forward: split each layer's columns, compute shards, concat, relu.
    let forward = |act: &[Vec<f64>], w: &[Vec<f64>], weights: &[u32]| {
        let cols = columns(w);
        let splits = weighted_split(w[0].len(), weights);
        let mut out = vec![Vec::new(); act.len()];
        let mut cur = 0;
        for n in splits {
            let shard = &cols[cur..cur + n];
            cur += n;
            let part = multiply_column_shard(act, shard).unwrap();
            for (i, row) in part.into_iter().enumerate() {
                out[i].extend(row);
            }
        }
        apply_relu(&mut out);
        out
    };
    let weights = [1u32, 3, 2];
    let a1 = forward(&input, &w0, &weights);
    let a2 = forward(&a1, &w1, &weights);

    assert_eq!(a2, reference);
}

#[test]
fn weighted_split_is_proportional_and_exact() {
    // Parts always sum to total; heavier weight gets more.
    let parts = weighted_split(100, &[1, 4]);
    assert_eq!(parts.iter().sum::<usize>(), 100);
    assert!(parts[1] > parts[0]);
    // More nodes than columns: still sums, no panic.
    let small = weighted_split(2, &[1, 1, 1, 1]);
    assert_eq!(small.iter().sum::<usize>(), 2);
    // Zero weights are floored to 1, never starved to a negative split.
    let floored = weighted_split(10, &[0, 0]);
    assert_eq!(floored.iter().sum::<usize>(), 10);
}

#[test]
fn rejects_non_rectangular_input() {
    let left = vec![vec![1.0, 2.0], vec![3.0]];
    let right = vec![vec![1.0], vec![2.0]];

    let error = multiply_parallel(&left, &right).expect_err("must reject ragged rows");
    assert!(error.to_string().contains("rectangular"));
}
