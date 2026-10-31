//! Micro-benchmarks of the API server's hot paths: the object store, patches
//! and JSON. Run with `cargo bench --bench micro` (add a filter, e.g.
//! `cargo bench --bench micro -- store/list`). Criterion keeps the last run in
//! target/criterion and prints the change against it; `--save-baseline main`
//! and `--baseline main` compare against a named run. See bench/PERF.md.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use litek8s::kube::patch::{merge_patch, strategic_merge_patch};
use litek8s::kube::store::{ObjectKey, Store};
use serde_json::{Value, json};

fn configmap(ns: &str, name: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": ns, "labels": {"app": "bench", "tier": "web"}},
        "data": {"key": "value", "config.yaml": "a: 1\nb: [1, 2, 3]\nc: {d: e}\n"}
    })
}

/// A Deployment shaped like what Helm charts produce: a few containers, env,
/// probes, resources, volumes.
fn deployment(name: &str) -> Value {
    let container = |n: &str| {
        json!({
            "name": n, "image": format!("registry.example.com/{n}:1.2.3"),
            "ports": [{"name": "http", "containerPort": 8080, "protocol": "TCP"}],
            "env": (0..8).map(|i| json!({"name": format!("VAR_{i}"), "value": format!("v{i}")})).collect::<Vec<_>>(),
            "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}, "limits": {"cpu": "500m", "memory": "256Mi"}},
            "readinessProbe": {"httpGet": {"path": "/healthz", "port": 8080}, "periodSeconds": 5},
            "volumeMounts": [{"name": "config", "mountPath": "/etc/app"}]
        })
    };
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": "default", "labels": {"app": name},
                     "annotations": {"deployment.kubernetes.io/revision": "3"}},
        "spec": {
            "replicas": 3,
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {
                    "containers": [container("app"), container("sidecar")],
                    "volumes": [{"name": "config", "configMap": {"name": "app-config"}}]
                }
            }
        }
    })
}

fn key(ns: &str, name: &str) -> ObjectKey {
    ObjectKey::new("", "configmaps", ns, name)
}

/// A store holding `n` ConfigMaps spread over 10 namespaces.
fn filled_store(n: usize) -> Store {
    let store = Store::open(None).unwrap();
    for i in 0..n {
        let ns = format!("ns-{}", i % 10);
        let name = format!("cm-{i}");
        store.create(key(&ns, &name), configmap(&ns, &name)).unwrap();
    }
    store
}

fn store(c: &mut Criterion) {
    let mut g = c.benchmark_group("store");

    // Create into an in-memory store (no WAL): bookkeeping + serialization.
    g.throughput(Throughput::Elements(1));
    g.bench_function("create/memory", |b| {
        let store = Store::open(None).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let name = format!("cm-{i}");
            store.create(key("default", &name), configmap("default", &name)).unwrap()
        })
    });

    // Create with the WAL on disk: what `apiserver` does by default.
    g.bench_function("create/wal", |b| {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let name = format!("cm-{i}");
            store.create(key("default", &name), configmap("default", &name)).unwrap()
        })
    });

    let big = filled_store(10_000);
    g.bench_function("get", |b| b.iter(|| big.get(black_box(&key("ns-3", "cm-5003"))).unwrap()));

    g.bench_function("update", |b| {
        let k = key("ns-3", "cm-5003");
        let obj = configmap("ns-3", "cm-5003");
        b.iter(|| big.update(k.clone(), obj.clone(), None).unwrap())
    });

    for n in [100usize, 1_000, 10_000] {
        let store = filled_store(n);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("list/all", n), &store, |b, s| {
            b.iter(|| s.list("", "configmaps", None))
        });
        g.throughput(Throughput::Elements((n / 10) as u64));
        g.bench_with_input(BenchmarkId::new("list/namespace", n), &store, |b, s| {
            b.iter(|| s.list("", "configmaps", Some("ns-3")))
        });
    }

    // One write fanned out to 100 watchers.
    g.throughput(Throughput::Elements(1));
    g.bench_function("watch_fanout/100", |b| {
        let store = Store::open(None).unwrap();
        let mut subs: Vec<_> = (0..100).map(|_| store.subscribe(None).unwrap()).collect();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            let name = format!("cm-{i}");
            store.create(key("default", &name), configmap("default", &name)).unwrap();
            for s in subs.iter_mut() {
                black_box(s.live.try_recv().unwrap());
            }
        })
    });

    // Replaying a WAL of 10k objects: server start time.
    g.sample_size(10);
    g.bench_function("open_wal/10000", |b| {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(Some(dir.path())).unwrap();
            for i in 0..10_000 {
                let name = format!("cm-{i}");
                store.create(key("default", &name), configmap("default", &name)).unwrap();
            }
        }
        b.iter(|| Store::open(Some(dir.path())).unwrap())
    });
    g.finish();
}

fn patch(c: &mut Criterion) {
    let mut g = c.benchmark_group("patch");
    let base = deployment("web");
    let scale = json!({"spec": {"replicas": 5}});
    let image = json!({"spec": {"template": {"spec": {"containers": [{"name": "app", "image": "registry.example.com/app:2.0.0"}]}}}});
    g.bench_function("merge/scale", |b| {
        b.iter_batched(|| base.clone(), |mut d| merge_patch(&mut d, black_box(&scale)), BatchSize::SmallInput)
    });
    g.bench_function("strategic/image", |b| {
        b.iter_batched(|| base.clone(), |mut d| strategic_merge_patch(&mut d, black_box(&image)), BatchSize::SmallInput)
    });
    g.bench_function("clone_deployment", |b| b.iter(|| black_box(&base).clone()));
    g.finish();
}

fn json(c: &mut Criterion) {
    let mut g = c.benchmark_group("json");
    let d = deployment("web");
    let bytes = serde_json::to_vec(&d).unwrap();
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.bench_function("serialize/deployment", |b| b.iter(|| serde_json::to_vec(black_box(&d)).unwrap()));
    g.bench_function("parse/deployment", |b| b.iter(|| serde_json::from_slice::<Value>(black_box(&bytes)).unwrap()));

    let list = json!({"apiVersion": "v1", "kind": "List", "items": (0..1000).map(|i| deployment(&format!("d{i}"))).collect::<Vec<_>>()});
    let list_bytes = serde_json::to_vec(&list).unwrap();
    g.throughput(Throughput::Bytes(list_bytes.len() as u64));
    g.sample_size(20);
    g.bench_function("serialize/list_1000", |b| b.iter(|| serde_json::to_vec(black_box(&list)).unwrap()));
    g.finish();
}

criterion_group!(benches, store, patch, json);
criterion_main!(benches);
