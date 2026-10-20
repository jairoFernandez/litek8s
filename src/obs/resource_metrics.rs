//! The resource metrics API (`metrics.k8s.io/v1beta1`, what metrics-server
//! serves) from the store: `kubectl top` and `litek8s top` read it.
//!
//! It is a built-in group of the API server, listed in discovery as long
//! as observability runs, with no APIService object (litek8s registers
//! none for its built-in groups). An APIService `v1beta1.metrics.k8s.io`
//! takes the group over: metrics-server's (Helm chart or manifest) points
//! at its Service and every request goes there (`kube/aggregator.rs`);
//! deleting it gives the group back to the store. Storing one here would
//! make `helm install metrics-server` fail: Helm does not adopt objects it
//! did not create.
//!
//! The numbers are metrics-server's: a container's CPU is the rate of
//! `container_cpu_usage_seconds_total` between its last two samples (the
//! window), its memory the last `container_memory_working_set_bytes`; a
//! node's are the rate of `node_cpu_usage_seconds_total` and the last
//! `node_memory_working_set_bytes`, the kubelet's names, which agents
//! sample from the node's own cgroup or the machine (`obs/node.rs`). Older
//! agents' nodes fall back to node-exporter's series, and a node with no
//! node series (an agent on macOS) adds up its pods.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::tsdb::{Db, Labels, MatchOp, Matcher, is_stale};
use crate::kube::registry::ResourceType;

pub const GROUP: &str = "metrics.k8s.io";
pub const VERSION: &str = "v1beta1";

/// The two resources, listed in discovery as an aggregated group version
/// (replaced by what an extension server lists when one takes over).
pub fn types() -> Vec<ResourceType> {
    let t = |kind: &str, plural: &str, namespaced: bool| ResourceType {
        group: GROUP.into(),
        version: VERSION.into(),
        kind: kind.into(),
        plural: plural.into(),
        singular: String::new(),
        namespaced,
        short_names: Vec::new(),
        categories: Vec::new(),
        has_status: false,
        has_scale: false,
        printer_columns: Vec::new(),
        custom: false,
        aggregated: true,
    };
    vec![t("NodeMetrics", "nodes", false), t("PodMetrics", "pods", true)]
}

/// One usage sample, as metrics-server reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Usage {
    /// When the newer of the two samples was taken (ms).
    pub ts: i64,
    /// Between the two samples (ms).
    pub window_ms: i64,
    pub cpu_cores: f64,
    pub memory_bytes: f64,
}

impl Usage {
    pub fn json(&self) -> Value {
        json!({"cpu": cpu_quantity(self.cpu_cores), "memory": memory_quantity(self.memory_bytes)})
    }
}

/// Nanocores, as metrics-server writes them (`123456n`).
pub fn cpu_quantity(cores: f64) -> String {
    format!("{}n", (cores.max(0.0) * 1e9).round() as u64)
}

/// Kibibytes when exact (as metrics-server writes them), else bytes.
pub fn memory_quantity(bytes: f64) -> String {
    let b = bytes.max(0.0).round() as u64;
    if b % 1024 == 0 { format!("{}Ki", b / 1024) } else { b.to_string() }
}

/// `20.5s`, as Go prints a duration.
pub fn window(ms: i64) -> String {
    let s = format!("{:.3}", ms as f64 / 1000.0);
    format!("{}s", s.trim_end_matches('0').trim_end_matches('.'))
}

pub fn timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn label<'a>(l: &'a Labels, k: &str) -> &'a str {
    l.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or_default()
}

/// Samples newer than this many intervals are current; older ones belong
/// to something that stopped reporting.
const FRESH_INTERVALS: i64 = 3;

/// The last two live samples of each series named `name` (plus `extra`
/// matchers): `(labels, (t1, v1), (t2, v2))`, or one sample twice when
/// that is all there is.
fn last_two(db: &Db, name: &str, extra: Vec<Matcher>, now: i64, interval: i64) -> Vec<(Labels, (i64, f64), (i64, f64))> {
    let mut m = vec![Matcher::eq("__name__", name)];
    m.extend(extra);
    let mint = now - (FRESH_INTERVALS + 2) * interval;
    let Ok(series) = db.select(&m, mint, now, 1) else { return Vec::new() };
    series
        .into_iter()
        .filter_map(|s| {
            let n = s.samples.len();
            let last = *s.samples.last()?;
            if is_stale(last.1) || last.0 < now - FRESH_INTERVALS * interval {
                return None;
            }
            let prev = if n >= 2 && !is_stale(s.samples[n - 2].1) { s.samples[n - 2] } else { last };
            Some((s.labels, prev, last))
        })
        .collect()
}

/// A counter's rate between two samples (none: one sample, or a reset).
fn rate(a: (i64, f64), b: (i64, f64)) -> Option<f64> {
    (b.0 > a.0 && b.1 >= a.1).then(|| (b.1 - a.1) / ((b.0 - a.0) as f64 / 1000.0))
}

/// Usage of every container with current samples, by (namespace, pod),
/// containers by name. A container needs two CPU samples (as with
/// metrics-server, a pod shows up one interval after it started).
pub fn containers(db: &Db, now: i64, interval_ms: i64, namespace: Option<&str>) -> BTreeMap<(String, String), BTreeMap<String, Usage>> {
    let extra = || {
        let mut m = vec![Matcher::new("container", MatchOp::Ne, "").expect("matcher"), Matcher::new("container", MatchOp::Ne, "POD").expect("matcher")];
        if let Some(ns) = namespace {
            m.push(Matcher::eq("namespace", ns));
        }
        m
    };
    let key = |l: &Labels| (label(l, "namespace").to_string(), label(l, "pod").to_string(), label(l, "container").to_string());
    let memory: HashMap<(String, String, String), f64> =
        last_two(db, "container_memory_working_set_bytes", extra(), now, interval_ms).into_iter().map(|(l, _, b)| (key(&l), b.1)).collect();
    let mut out: BTreeMap<(String, String), BTreeMap<String, Usage>> = BTreeMap::new();
    for (l, a, b) in last_two(db, "container_cpu_usage_seconds_total", extra(), now, interval_ms) {
        let Some(cpu) = rate(a, b) else { continue };
        let (ns, pod, container) = key(&l);
        let memory_bytes = memory.get(&(ns.clone(), pod.clone(), container.clone())).copied().unwrap_or(0.0);
        out.entry((ns, pod)).or_default().insert(container, Usage { ts: b.0, window_ms: b.0 - a.0, cpu_cores: cpu, memory_bytes });
    }
    out
}

/// CPU modes that are not work.
const IDLE_MODES: [&str; 3] = ["idle", "iowait", "steal"];

/// Usage of every node with current samples.
pub fn nodes(db: &Db, now: i64, interval_ms: i64) -> BTreeMap<String, Usage> {
    let mut out: BTreeMap<String, Usage> = BTreeMap::new();
    // What agents report as the kubelet does (its own cgroup in a dev cell).
    let memory: HashMap<String, f64> =
        last_two(db, "node_memory_working_set_bytes", Vec::new(), now, interval_ms).into_iter().map(|(l, _, b)| (label(&l, "node").to_string(), b.1)).collect();
    for (l, a, b) in last_two(db, "node_cpu_usage_seconds_total", Vec::new(), now, interval_ms) {
        let node = label(&l, "node").to_string();
        if let (Some(cpu), Some(m)) = (rate(a, b), memory.get(&node)) {
            out.insert(node, Usage { ts: b.0, window_ms: b.0 - a.0, cpu_cores: cpu, memory_bytes: *m });
        }
    }
    // Agents from before those series: node-exporter's.
    let mut older: BTreeMap<String, Usage> = BTreeMap::new();
    for (l, a, b) in last_two(db, "node_cpu_seconds_total", Vec::new(), now, interval_ms) {
        if out.contains_key(label(&l, "node")) {
            continue;
        }
        if IDLE_MODES.contains(&label(&l, "mode")) {
            continue;
        }
        let Some(cpu) = rate(a, b) else { continue };
        let u = older.entry(label(&l, "node").to_string()).or_insert(Usage { ts: b.0, window_ms: b.0 - a.0, cpu_cores: 0.0, memory_bytes: 0.0 });
        u.cpu_cores += cpu;
    }
    let latest = |name: &str| -> HashMap<String, f64> {
        last_two(db, name, Vec::new(), now, interval_ms).into_iter().map(|(l, _, b)| (label(&l, "node").to_string(), b.1)).collect()
    };
    let (total, available) = (latest("node_memory_MemTotal_bytes"), latest("node_memory_MemAvailable_bytes"));
    for (node, u) in older.iter_mut() {
        if let (Some(t), Some(a)) = (total.get(node), available.get(node)) {
            u.memory_bytes = (t - a).max(0.0);
        }
    }
    out.extend(older);
    // Nodes without node series (no /proc: a Docker agent on macOS): the
    // sum of their containers.
    let mut sums: BTreeMap<String, Usage> = BTreeMap::new();
    let extra = vec![Matcher::new("container", MatchOp::Ne, "").expect("matcher")];
    let memory: HashMap<(String, String, String), f64> = last_two(db, "container_memory_working_set_bytes", extra.clone(), now, interval_ms)
        .into_iter()
        .map(|(l, _, b)| ((label(&l, "node").to_string(), label(&l, "pod").to_string(), label(&l, "container").to_string()), b.1))
        .collect();
    for (l, a, b) in last_two(db, "container_cpu_usage_seconds_total", extra, now, interval_ms) {
        let node = label(&l, "node").to_string();
        if out.contains_key(&node) {
            continue;
        }
        let u = sums.entry(node.clone()).or_insert(Usage { ts: b.0, window_ms: (b.0 - a.0).max(0), cpu_cores: 0.0, memory_bytes: 0.0 });
        u.cpu_cores += rate(a, b).unwrap_or(0.0);
        u.memory_bytes += memory.get(&(node, label(&l, "pod").to_string(), label(&l, "container").to_string())).copied().unwrap_or(0.0);
    }
    out.extend(sums.into_iter().filter(|(_, u)| u.window_ms > 0));
    out
}

fn labels_of(obj: &Value) -> Value {
    obj.pointer("/metadata/labels").cloned().unwrap_or_else(|| json!({}))
}

/// A NodeMetrics object for `node` (the Node object gives its labels).
pub fn node_metrics(node: &Value, u: &Usage, now: i64) -> Value {
    json!({
        "kind": "NodeMetrics",
        "apiVersion": format!("{GROUP}/{VERSION}"),
        "metadata": {"name": node.pointer("/metadata/name").cloned().unwrap_or_default(), "creationTimestamp": timestamp(now), "labels": labels_of(node)},
        "timestamp": timestamp(u.ts),
        "window": window(u.window_ms),
        "usage": u.json(),
    })
}

/// A PodMetrics object for `pod`, its containers in the pod's order.
pub fn pod_metrics(pod: &Value, containers: &BTreeMap<String, Usage>, now: i64) -> Value {
    let order: Vec<&str> = pod.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c["name"].as_str()).collect();
    let mut names: Vec<&String> = containers.keys().collect();
    names.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
    let first = containers.values().max_by_key(|u| u.ts).copied().unwrap_or(Usage { ts: now, window_ms: 0, cpu_cores: 0.0, memory_bytes: 0.0 });
    json!({
        "kind": "PodMetrics",
        "apiVersion": format!("{GROUP}/{VERSION}"),
        "metadata": {
            "name": pod.pointer("/metadata/name").cloned().unwrap_or_default(),
            "namespace": pod.pointer("/metadata/namespace").cloned().unwrap_or_default(),
            "creationTimestamp": timestamp(now),
            "labels": labels_of(pod),
        },
        "timestamp": timestamp(first.ts),
        "window": window(first.window_ms),
        "containers": names.iter().map(|n| json!({"name": n, "usage": containers[*n].json()})).collect::<Vec<_>>(),
    })
}

/// Server-side table rows for `kubectl get nodes.metrics.k8s.io` (and
/// pods): name, CPU, memory, window, as metrics-server prints them.
pub fn table(kind: &str, items: &[Value]) -> Value {
    let row = |o: &Value| {
        let (cpu, mem) = match kind {
            "NodeMetrics" => (o["usage"]["cpu"].clone(), o["usage"]["memory"].clone()),
            _ => {
                let parse = |q: &Value| crate::kube::quantity::milli_of(q).unwrap_or(0) as f64 / 1000.0;
                let cs = o["containers"].as_array().cloned().unwrap_or_default();
                let cpu: f64 = cs.iter().map(|c| parse(&c["usage"]["cpu"])).sum();
                let mem: f64 = cs.iter().map(|c| parse(&c["usage"]["memory"])).sum();
                (json!(cpu_quantity(cpu)), json!(memory_quantity(mem)))
            }
        };
        json!({
            "cells": [o["metadata"]["name"], cpu, mem, o["window"]],
            "object": {"kind": "PartialObjectMetadata", "apiVersion": "meta.k8s.io/v1", "metadata": o["metadata"]},
        })
    };
    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {},
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name", "description": "Name of the resource", "priority": 0},
            {"name": "CPU", "type": "string", "format": "", "description": "CPU usage", "priority": 0},
            {"name": "Memory", "type": "string", "format": "", "description": "Memory usage", "priority": 0},
            {"name": "Window", "type": "string", "format": "", "description": "Time window of the sample", "priority": 0},
        ],
        "rows": items.iter().map(row).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::tsdb;

    fn l(pairs: &[(&str, &str)]) -> Labels {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn quantities_like_metrics_server() {
        assert_eq!(cpu_quantity(0.25), "250000000n");
        assert_eq!(memory_quantity(4096.0), "4Ki");
        assert_eq!(memory_quantity(1000.0), "1000");
        assert_eq!(window(10_000), "10s");
        assert_eq!(window(10_250), "10.25s");
    }

    #[test]
    fn usage_from_the_last_two_samples() {
        let db = Db::open(tsdb::Config::default()).unwrap();
        let now = tsdb::now_ms();
        let c = |name: &str, pod: &str| l(&[("__name__", name), ("node", "n1"), ("namespace", "default"), ("pod", pod), ("container", "app")]);
        let n = |mode: &str| l(&[("__name__", "node_cpu_seconds_total"), ("node", "n1"), ("mode", mode)]);
        let m = |name: &str| l(&[("__name__", name), ("node", "n1")]);
        for (i, t) in [now - 20_000, now - 10_000].into_iter().enumerate() {
            let i = i as f64;
            db.append(
                "node/n1",
                t,
                vec![
                    (c("container_cpu_usage_seconds_total", "web"), 100.0 + i * 5.0),
                    (c("container_memory_working_set_bytes", "web"), 8192.0),
                    (n("user"), 1000.0 + i * 15.0),
                    (n("system"), 500.0 + i * 5.0),
                    (n("idle"), 9000.0 + i * 60.0),
                    (m("node_memory_MemTotal_bytes"), 4096.0 * 1024.0),
                    (m("node_memory_MemAvailable_bytes"), 1024.0 * 1024.0),
                ],
            )
            .unwrap();
            // An agent with the kubelet's series: those win.
            let k = |name: &str| l(&[("__name__", name), ("node", "n2")]);
            let busy = l(&[("__name__", "node_cpu_seconds_total"), ("node", "n2"), ("mode", "user")]);
            db.append("node/n2", t, vec![(k("node_cpu_usage_seconds_total"), 7.0 + i * 2.5), (k("node_memory_working_set_bytes"), 2048.0), (busy, 99.0 + i * 40.0)]).unwrap();
        }
        let pods = containers(&db, now, 10_000, None);
        let u = pods[&("default".to_string(), "web".to_string())]["app"];
        assert_eq!((u.cpu_cores, u.memory_bytes, u.window_ms), (0.5, 8192.0, 10_000));
        assert!(containers(&db, now, 10_000, Some("other")).is_empty());
        let nodes = nodes(&db, now, 10_000);
        assert_eq!((nodes["n1"].cpu_cores, nodes["n1"].memory_bytes), (2.0, 3.0 * 1024.0 * 1024.0));
        assert_eq!((nodes["n2"].cpu_cores, nodes["n2"].memory_bytes), (0.25, 2048.0));
        // Too old: nothing current.
        assert!(containers(&db, now + 60_000, 10_000, None).is_empty());
    }

    #[test]
    fn nodes_without_node_series_add_up_their_pods() {
        let db = Db::open(tsdb::Config::default()).unwrap();
        let now = tsdb::now_ms();
        let c = |name: &str, pod: &str| l(&[("__name__", name), ("node", "mac"), ("namespace", "default"), ("pod", pod), ("container", "app")]);
        for (i, t) in [now - 20_000, now - 10_000].into_iter().enumerate() {
            let i = i as f64;
            db.append(
                "node/mac",
                t,
                vec![
                    (c("container_cpu_usage_seconds_total", "a"), 10.0 + i),
                    (c("container_cpu_usage_seconds_total", "b"), 10.0 + i * 2.0),
                    (c("container_memory_working_set_bytes", "a"), 1024.0),
                    (c("container_memory_working_set_bytes", "b"), 2048.0),
                ],
            )
            .unwrap();
        }
        let nodes = nodes(&db, now, 10_000);
        assert!((nodes["mac"].cpu_cores - 0.3).abs() < 1e-9);
        assert_eq!(nodes["mac"].memory_bytes, 3072.0);
    }
}
