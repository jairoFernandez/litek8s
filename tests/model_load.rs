use litek8s::model::{inspect, load_tensor_2d};
use safetensors::tensor::{serialize, Dtype, TensorView};
use std::collections::HashMap;

/// Build a safetensors blob with one F32 and one F16 tensor, write it to a
/// per-test temp file (so parallel tests don't clobber each other), return path.
fn write_fixture(tag: &str) -> std::path::PathBuf {
    // 2x3 F32 weight.
    let f32_vals: [f32; 6] = [1.0, -2.0, 3.5, 0.0, -0.25, 4.0];
    let mut f32_bytes = Vec::new();
    for v in f32_vals {
        f32_bytes.extend_from_slice(&v.to_le_bytes());
    }
    // 2x2 F16 weight.
    let f16_vals: [half::f16; 4] = [
        half::f16::from_f32(0.5),
        half::f16::from_f32(-1.5),
        half::f16::from_f32(2.0),
        half::f16::from_f32(0.125),
    ];
    let mut f16_bytes = Vec::new();
    for v in f16_vals {
        f16_bytes.extend_from_slice(&v.to_le_bytes());
    }

    let w32 = TensorView::new(Dtype::F32, vec![2, 3], &f32_bytes).unwrap();
    let w16 = TensorView::new(Dtype::F16, vec![2, 2], &f16_bytes).unwrap();
    let mut tensors: HashMap<String, TensorView> = HashMap::new();
    tensors.insert("w32".to_string(), w32);
    tensors.insert("w16".to_string(), w16);

    let bytes = serialize(&tensors, None).unwrap();
    let path = std::env::temp_dir().join(format!(
        "litek8s-model-test-{}-{tag}.safetensors",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn inspect_lists_tensors_with_dtype_and_shape() {
    let path = write_fixture("inspect");
    let infos = inspect(&path).unwrap();
    let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"w32"));
    assert!(names.contains(&"w16"));
    let w32 = infos.iter().find(|i| i.name == "w32").unwrap();
    assert_eq!(w32.dtype, "F32");
    assert_eq!(w32.shape, vec![2, 3]);
    std::fs::remove_file(&path).ok();
}

#[test]
fn loads_f32_tensor_exactly() {
    let path = write_fixture("f32");
    let m = load_tensor_2d(&path, "w32").unwrap();
    assert_eq!(m, vec![vec![1.0, -2.0, 3.5], vec![0.0, -0.25, 4.0]]);
    std::fs::remove_file(&path).ok();
}

#[test]
fn loads_f16_tensor_upcast_to_f64() {
    let path = write_fixture("f16");
    let m = load_tensor_2d(&path, "w16").unwrap();
    // f16 represents these exactly, so the upcast is exact too.
    assert_eq!(m, vec![vec![0.5, -1.5], vec![2.0, 0.125]]);
    std::fs::remove_file(&path).ok();
}

#[test]
fn rejects_missing_tensor() {
    let path = write_fixture("missing");
    assert!(load_tensor_2d(&path, "nope").is_err());
    std::fs::remove_file(&path).ok();
}
