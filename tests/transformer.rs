use litek8s::transformer::{
    add, causal_self_attention, rms_norm, rope_tables, silu, softmax, swiglu_ffn, transformer_block,
    BlockConfig, BlockWeights,
};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

fn flat(m: &[Vec<f64>]) -> Vec<f64> {
    m.iter().flatten().copied().collect()
}

#[test]
fn rms_norm_matches_torch() {
    let x = vec![vec![1.0, 2.0, 3.0, 4.0], vec![0.5, -1.0, 2.0, 0.0]];
    let w = vec![1.0, 1.0, 1.0, 1.0];
    let out = flat(&rms_norm(&x, &w, 1e-6));
    let expect = [
        0.3651483654975891,
        0.7302967309951782,
        1.095445156097412,
        1.4605934619903564,
        0.43643563985824585,
        -0.8728712797164917,
        1.7457425594329834,
        0.0,
    ];
    for (a, b) in out.iter().zip(expect.iter()) {
        assert!(close(*a, *b), "{a} vs {b}");
    }
}

#[test]
fn softmax_matches_torch() {
    let out = softmax(&[1.0, 2.0, 3.0]);
    let expect = [0.09003057330846786, 0.2447284758090973, 0.6652409434318542];
    for (a, b) in out.iter().zip(expect.iter()) {
        assert!(close(*a, *b), "{a} vs {b}");
    }
}

#[test]
fn silu_matches_torch() {
    let xs = [1.0, -1.0, 2.0, 0.0];
    let expect = [
        0.7310585975646973,
        -0.2689414322376251,
        1.7615940570831299,
        0.0,
    ];
    for (x, e) in xs.iter().zip(expect.iter()) {
        assert!(close(silu(*x), *e), "{} vs {e}", silu(*x));
    }
}

#[test]
fn causal_attention_with_rope_matches_torch() {
    let seq = 3;
    let nh = 2;
    let hd = 4;
    let make = |scale: f64| -> Vec<Vec<f64>> {
        (0..seq)
            .map(|t| (0..nh * hd).map(|i| (t * nh * hd + i) as f64 * scale).collect())
            .collect()
    };
    let q = make(0.01);
    let k = make(0.02);
    let v = make(0.03);
    let (cos, sin) = rope_tables(seq, hd, 10000.0);
    let out = flat(&causal_self_attention(&q, &k, &v, nh, hd, &cos, &sin));

    let expect = [
        0.0, 0.03, 0.06, 0.09, 0.12, 0.15, 0.18, 0.21, 0.121797, 0.151797, 0.181797, 0.211797,
        0.242874, 0.272874, 0.302874, 0.332874, 0.249161, 0.279161, 0.309161, 0.339161, 0.373202,
        0.403202, 0.433202, 0.463202,
    ];
    assert_eq!(out.len(), expect.len());
    for (a, b) in out.iter().zip(expect.iter()) {
        assert!(close(*a, *b), "{a} vs {b}");
    }
}

#[test]
fn swiglu_runs_and_shapes_match() {
    // Shape + determinism check (numeric reference covered by primitives above).
    let x = vec![vec![0.1, 0.2, 0.3]; 2]; // 2 tokens x dim 3
    let w_gate = vec![vec![0.1, 0.2, 0.3, 0.4]; 3]; // dim x hidden(4)
    let w_up = vec![vec![0.5, 0.6, 0.7, 0.8]; 3];
    let w_down = vec![vec![0.1, 0.2, 0.3]; 4]; // hidden x dim
    let out = swiglu_ffn(&x, &w_gate, &w_up, &w_down).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].len(), 3);
    // Both token rows identical (identical input rows) — determinism.
    assert_eq!(out[0], out[1]);
}

#[test]
fn full_block_matches_torch() {
    // dim = n_heads * head_dim = 4, seq = 3, hidden = 6.
    let dim = 4;
    let seq = 3;
    let hidden = 6;
    let mat = |r: usize, c: usize, sc: f64| -> Vec<Vec<f64>> {
        (0..r)
            .map(|i| (0..c).map(|j| (i * c + j) as f64 * sc).collect())
            .collect()
    };
    let x = mat(seq, dim, 0.1);
    let w = BlockWeights {
        attn_norm: vec![1.0; dim],
        wq: mat(dim, dim, 0.01),
        wk: mat(dim, dim, 0.02),
        wv: mat(dim, dim, 0.03),
        wo: mat(dim, dim, 0.04),
        ffn_norm: vec![1.0; dim],
        w_gate: mat(dim, hidden, 0.01),
        w_up: mat(dim, hidden, 0.02),
        w_down: mat(hidden, dim, 0.03),
    };
    let cfg = BlockConfig {
        n_heads: 2,
        head_dim: 2,
        rope_theta: 10000.0,
        eps: 1e-6,
    };
    let out = flat(&transformer_block(&x, &w, cfg).unwrap());
    let expect = [
        1.774958, 2.100108, 2.425258, 2.750409, 2.136479, 2.455212, 2.773944, 3.092677, 2.502667,
        2.816264, 3.12986, 3.443456,
    ];
    assert_eq!(out.len(), expect.len());
    for (a, b) in out.iter().zip(expect.iter()) {
        assert!(close(*a, *b), "{a} vs {b}");
    }
}

#[test]
fn residual_add() {
    let a = vec![vec![1.0, 2.0]];
    let b = vec![vec![0.5, -1.0]];
    assert_eq!(add(&a, &b), vec![vec![1.5, 1.0]]);
}
