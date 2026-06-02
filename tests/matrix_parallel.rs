use litek8s::matrix::{
    deterministic_matrix, multiply_band, multiply_parallel, multiply_sequential,
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
fn rejects_non_rectangular_input() {
    let left = vec![vec![1.0, 2.0], vec![3.0]];
    let right = vec![vec![1.0], vec![2.0]];

    let error = multiply_parallel(&left, &right).expect_err("must reject ragged rows");
    assert!(error.to_string().contains("rectangular"));
}
