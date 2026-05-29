use litek8s::matrix::{multiply_parallel, multiply_sequential};

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
fn rejects_non_rectangular_input() {
    let left = vec![vec![1.0, 2.0], vec![3.0]];
    let right = vec![vec![1.0], vec![2.0]];

    let error = multiply_parallel(&left, &right).expect_err("must reject ragged rows");
    assert!(error.to_string().contains("rectangular"));
}
