//! Rows as the API server prints them: server-side Tables
//! (`as=Table;g=meta.k8s.io`, the columns kubectl shows, CRDs' printer
//! columns included) with each row's metadata, kept compact (cells as
//! strings, labels and owners; no spec or status). A server that cannot
//! print a type gets Name and Age from plain lists.

use std::cmp::Ordering;
use std::sync::Arc;

use serde_json::Value;

/// The `Accept` of lists and watches.
pub const TABLE_ACCEPT: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// `string`, `integer`, `number`, `boolean`, `date`.
    pub kind: String,
    /// 0: always shown; more: only in the wide view (as `-o wide`).
    pub priority: i64,
}

impl Column {
    pub fn new(name: &str, priority: i64) -> Column {
        Column { name: name.into(), kind: "string".into(), priority }
    }

    /// The Age column is drawn from the creation time, so it moves on.
    pub fn is_age(&self) -> bool {
        self.name.eq_ignore_ascii_case("age") || (self.kind == "date" && self.name.eq_ignore_ascii_case("created"))
    }
}

pub use crate::ui::theme::Health;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub controller: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub ns: String,
    pub name: String,
    pub uid: String,
    /// One per column.
    pub cells: Vec<String>,
    pub created: i64,
    pub deleting: bool,
    /// `{"metadata": {"labels": {...}}}`, for label selectors.
    pub labels: Value,
    pub owners: Vec<Owner>,
    pub health: Health,
    /// Lowercase name and cells, what `/` searches.
    pub haystack: String,
}

impl Row {
    pub fn key(&self) -> String {
        key(&self.ns, &self.name)
    }

    pub fn label(&self, k: &str) -> Option<&str> {
        self.labels.pointer("/metadata/labels")?.get(k)?.as_str()
    }

    pub fn cell(&self, columns: &[Column], name: &str) -> Option<&str> {
        columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).and_then(|i| self.cells.get(i)).map(String::as_str)
    }
}

pub fn key(ns: &str, name: &str) -> String {
    if ns.is_empty() { name.to_string() } else { format!("{ns}/{name}") }
}

/// What an informer holds for one list: columns and rows by key.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub columns: Arc<Vec<Column>>,
    /// Sorted by key.
    pub rows: Vec<Arc<Row>>,
    /// The list arrived (an empty one is empty, not loading).
    pub synced: bool,
    pub error: Option<String>,
}

/// The columns of a Table reply; `None` for a plain list.
pub fn columns(doc: &Value) -> Option<Vec<Column>> {
    if doc["kind"].as_str() != Some("Table") {
        return None;
    }
    Some(
        doc["columnDefinitions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Column { name: c["name"].as_str().unwrap_or_default().to_string(), kind: c["type"].as_str().unwrap_or("string").to_string(), priority: c["priority"].as_i64().unwrap_or(0) })
            .collect(),
    )
}

/// A list reply as the informer reads it: a Table's columns and rows, or
/// a plain list's items; of each object only the metadata the UI uses (no
/// annotations, no managedFields), so big lists stay small.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListDoc {
    pub kind: String,
    pub column_definitions: Vec<ColDoc>,
    pub rows: Vec<RowDoc>,
    pub items: Vec<ObjDoc>,
    pub metadata: ListMeta,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct ColDoc {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub priority: i64,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct RowDoc {
    pub cells: Vec<Value>,
    pub object: ObjDoc,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct ObjDoc {
    pub metadata: MetaDoc,
    /// Kept only for kinds whose plain rows print from it (services).
    pub spec: Value,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MetaDoc {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub creation_timestamp: Option<String>,
    pub deletion_timestamp: Option<String>,
    pub labels: Option<serde_json::Map<String, Value>>,
    pub owner_references: Vec<OwnerDoc>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct OwnerDoc {
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub controller: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListMeta {
    pub resource_version: String,
    #[serde(rename = "continue")]
    pub cont: String,
}

impl ListDoc {
    pub fn columns(&self) -> Option<Vec<Column>> {
        (self.kind == "Table").then(|| self.column_definitions.iter().map(|c| Column { name: c.name.clone(), kind: if c.kind.is_empty() { "string".into() } else { c.kind.clone() }, priority: c.priority }).collect())
    }

    /// Its rows: a Table's, or a plain list's (Name, Age).
    pub fn into_rows(self, columns: &[Column], kind: &str) -> Vec<Row> {
        if self.kind == "Table" {
            self.rows.into_iter().map(|r| row_of(r.object.metadata, r.cells.iter().map(cell_text).collect(), columns, kind)).collect()
        } else {
            let cols = plain_columns_for(kind);
            self.items.into_iter().map(|o| {
                let cells = plain_cells(kind, &o.metadata.name, &o.spec);
                row_of(o.metadata, cells, &cols, kind)
            }).collect()
        }
    }
}

fn row_of(m: MetaDoc, cells: Vec<String>, columns: &[Column], kind: &str) -> Row {
    let labels = match m.labels {
        Some(l) => serde_json::json!({"metadata": {"labels": l}}),
        None => Value::Null,
    };
    let mut r = Row {
        ns: m.namespace,
        name: m.name,
        uid: m.uid,
        created: timestamp(m.creation_timestamp.as_deref()),
        deleting: m.deletion_timestamp.is_some(),
        labels,
        owners: m.owner_references.into_iter().map(|o| Owner { kind: o.kind, name: o.name, uid: o.uid, controller: o.controller.unwrap_or(false) }).collect(),
        health: Health::Ok,
        haystack: String::new(),
        cells,
    };
    finish(&mut r, columns, kind);
    r
}

fn finish(r: &mut Row, columns: &[Column], kind: &str) {
    if r.name.is_empty() {
        r.name = r.cells.first().cloned().unwrap_or_default();
    }
    r.health = health(kind, columns, r);
    let mut hay = r.name.to_lowercase();
    for c in &r.cells {
        hay.push(' ');
        hay.push_str(&c.to_lowercase());
    }
    r.haystack = hay;
}

/// The columns without a Table: what every object has.
pub fn plain_columns() -> Vec<Column> {
    vec![Column::new("Name", 0), Column::new("Age", 0)]
}

/// Rows of a Table reply (or of a Table in a watch event), for `kind` (to
/// judge health).
pub fn table_rows(doc: &Value, columns: &[Column], kind: &str) -> Vec<Row> {
    doc["rows"].as_array().into_iter().flatten().map(|r| row(&r["object"], r["cells"].as_array().map(Vec::as_slice).unwrap_or(&[]), columns, kind)).collect()
}

/// The columns of plain objects of `kind`: Name and Age, and for
/// services kubectl's (Selector too), printed here from their spec.
pub fn plain_columns_for(kind: &str) -> Vec<Column> {
    match kind {
        "Service" => ["Name", "Type", "Cluster-IP", "External-IP", "Port(s)", "Age", "Selector"].iter().map(|n| Column::new(n, if *n == "Selector" { 1 } else { 0 })).collect(),
        _ => plain_columns(),
    }
}

fn plain_cells(kind: &str, name: &str, spec: &Value) -> Vec<String> {
    match kind {
        "Service" => {
            let ports: Vec<String> = spec["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    let proto = p["protocol"].as_str().unwrap_or("TCP");
                    match p["nodePort"].as_u64() {
                        Some(np) => format!("{}:{np}/{proto}", p["port"]),
                        None => format!("{}/{proto}", p["port"]),
                    }
                })
                .collect();
            let sel: Vec<String> = spec["selector"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect();
            let or_none = |v: String| if v.is_empty() { "<none>".to_string() } else { v };
            vec![
                name.to_string(),
                spec["type"].as_str().unwrap_or("ClusterIP").to_string(),
                or_none(spec["clusterIP"].as_str().unwrap_or_default().to_string()),
                "<none>".into(),
                or_none(ports.join(",")),
                String::new(),
                or_none(sel.join(",")),
            ]
        }
        _ => vec![name.to_string(), String::new()],
    }
}

/// A row of a plain object (no Table).
pub fn plain_row(obj: &Value, kind: &str) -> Row {
    let cols = plain_columns_for(kind);
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let cells: Vec<Value> = plain_cells(kind, name, &obj["spec"]).into_iter().map(Value::String).collect();
    row(obj, &cells, &cols, kind)
}

fn row(obj: &Value, cells: &[Value], columns: &[Column], kind: &str) -> Row {
    let meta = &obj["metadata"];
    let s = |k: &str| meta[k].as_str().unwrap_or_default().to_string();
    let cells: Vec<String> = cells.iter().map(cell_text).collect();
    let owners = meta["ownerReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| Owner {
            kind: o["kind"].as_str().unwrap_or_default().into(),
            name: o["name"].as_str().unwrap_or_default().into(),
            uid: o["uid"].as_str().unwrap_or_default().into(),
            controller: o["controller"].as_bool().unwrap_or(false),
        })
        .collect();
    let labels = match meta.get("labels") {
        Some(l) if l.is_object() => serde_json::json!({"metadata": {"labels": l}}),
        _ => Value::Null,
    };
    let mut r = Row {
        ns: s("namespace"),
        name: s("name"),
        uid: s("uid"),
        created: timestamp(meta["creationTimestamp"].as_str()),
        deleting: meta.get("deletionTimestamp").is_some_and(|d| !d.is_null()),
        labels,
        owners,
        health: Health::Ok,
        haystack: String::new(),
        cells,
    };
    finish(&mut r, columns, kind);
    r
}

pub fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => a.iter().map(cell_text).collect::<Vec<_>>().join(","),
        Value::Object(_) => v.to_string(),
    }
}

pub fn timestamp(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t| t.timestamp()).unwrap_or(0)
}

/// `2/3` → (2, 3).
fn fraction(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// How a row looks: the pod states kubectl prints, a workload's ready
/// count, a node's conditions, a status word.
fn health(kind: &str, columns: &[Column], r: &Row) -> Health {
    if r.deleting {
        return Health::Warn;
    }
    let cell = |n: &str| r.cell(columns, n).unwrap_or_default();
    match kind {
        "Pod" => {
            let status = cell("Status");
            match status {
                "Completed" | "Succeeded" => Health::Done,
                "Running" => match fraction(cell("Ready")) {
                    Some((a, b)) if a < b => Health::Warn,
                    _ => Health::Ok,
                },
                "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "" => Health::Warn,
                s if s.starts_with("Init:") && !s.contains("Err") && !s.contains("Crash") => Health::Warn,
                _ => Health::Bad,
            }
        }
        "Node" => {
            let s = cell("Status");
            if s.contains("NotReady") || s.contains("Unknown") {
                Health::Bad
            } else if s.contains("SchedulingDisabled") {
                Health::Warn
            } else {
                Health::Ok
            }
        }
        "Job" => {
            let s = cell("Status");
            if s.contains("Fail") {
                Health::Bad
            } else if s == "Complete" || fraction(cell("Completions")).is_some_and(|(a, b)| a >= b) {
                Health::Done
            } else {
                Health::Warn
            }
        }
        "Event" => {
            if cell("Type") == "Warning" {
                Health::Warn
            } else {
                Health::Ok
            }
        }
        // Argo CD's Applications: their Sync Status and Health Status columns.
        "Application" if r.cell(columns, "Health Status").is_some() => super::gitops::health_of(cell("Health Status"), cell("Sync Status")),
        "PersistentVolumeClaim" | "PersistentVolume" => match cell("Status") {
            "Bound" => Health::Ok,
            "Pending" | "Released" => Health::Warn,
            "" => Health::Ok,
            _ => Health::Bad,
        },
        _ => {
            if let Some((a, b)) = fraction(cell("Ready")) {
                return if a < b { Health::Warn } else { Health::Ok };
            }
            let status = cell("Status").to_lowercase();
            if ["fail", "error", "crash", "backoff", "unknown", "notready"].iter().any(|w| status.contains(w)) {
                Health::Bad
            } else if ["pending", "progressing", "terminating"].iter().any(|w| status.contains(w)) {
                Health::Warn
            } else {
                Health::Ok
            }
        }
    }
}

/// Compare two cells of a column the way people read them: numbers as
/// numbers (`10` after `9`), `2/3` by its counts, quantities (`512Mi`,
/// `250m`) by size, ages by time (the caller passes creation times).
pub fn compare_cells(a: &str, b: &str) -> Ordering {
    if let (Some(x), Some(y)) = (fraction(a), fraction(b)) {
        return x.cmp(&y);
    }
    if let (Some(x), Some(y)) = (quantity(a), quantity(b)) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    natural(a, b)
}

/// `250m` → 0.25, `128Mi` → 134217728, `3` → 3, `1.5` → 1.5, `12%` → 12;
/// with a trailing ` (5m ago)` (restarts) the leading number.
pub fn quantity(s: &str) -> Option<f64> {
    let s = s.split_whitespace().next()?;
    let s = s.trim_end_matches('%');
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let m = match unit {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" | "K" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024f64.powi(4),
        "P" => 1e15,
        "E" => 1e18,
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        _ => return None,
    };
    Some(n * m)
}

/// Strings with digit runs compared as numbers: `web-2` < `web-10`.
pub fn natural(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let i = x.iter().position(|c| !c.is_ascii_digit()).unwrap_or(x.len());
                let j = y.iter().position(|c| !c.is_ascii_digit()).unwrap_or(y.len());
                let (na, nb) = (trim_zeros(&x[..i]), trim_zeros(&y[..j]));
                let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if o != Ordering::Equal {
                    return o;
                }
                x = &x[i..];
                y = &y[j..];
            }
            (Some(c), Some(d)) => {
                let o = c.to_ascii_lowercase().cmp(&d.to_ascii_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x = &x[1..];
                y = &y[1..];
            }
        }
    }
}

fn trim_zeros(s: &[u8]) -> &[u8] {
    let i = s.iter().position(|c| *c != b'0').unwrap_or(s.len());
    &s[i..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quantities_in_every_suffix() {
        // metrics.k8s.io answers CPU in nanocores.
        assert_eq!(quantity("258000000n"), Some(0.258));
        assert_eq!(quantity("1500u"), Some(0.0015));
        assert_eq!(quantity("250m"), Some(0.25));
        assert_eq!(quantity("1277212Ki"), Some(1277212.0 * 1024.0));
        assert_eq!(quantity("2Pi"), Some(2.0 * 1024f64.powi(5)));
        assert_eq!(quantity("3x"), None);
    }

    fn pod_table() -> Value {
        json!({"kind": "Table", "columnDefinitions": [
            {"name": "Name", "type": "string", "priority": 0}, {"name": "Ready", "type": "string", "priority": 0},
            {"name": "Status", "type": "string", "priority": 0}, {"name": "Restarts", "type": "string", "priority": 0},
            {"name": "Age", "type": "string", "priority": 0}, {"name": "IP", "type": "string", "priority": 1}],
          "rows": [
            {"cells": ["web-1", "1/1", "Running", 0, "5m", "10.0.0.1"], "object": {"metadata": {"name": "web-1", "namespace": "default", "uid": "u1", "creationTimestamp": "2026-01-01T00:00:00Z", "labels": {"app": "web"}, "ownerReferences": [{"kind": "ReplicaSet", "name": "web-7d9", "uid": "rs1", "controller": true}]}}},
            {"cells": ["db-0", "0/1", "CrashLoopBackOff", "4 (2m ago)", "1h", null], "object": {"metadata": {"name": "db-0", "namespace": "default"}}},
            {"cells": ["init", "0/1", "Init:0/1", 0, "1h", null], "object": {"metadata": {"name": "init", "namespace": "default"}}}
          ]})
    }

    #[test]
    fn table_rows_keep_cells_metadata_and_health() {
        let t = pod_table();
        let cols = columns(&t).unwrap();
        assert_eq!(cols.len(), 6);
        assert_eq!(cols[5].priority, 1);
        let rows = table_rows(&t, &cols, "Pod");
        assert_eq!(rows[0].key(), "default/web-1");
        assert_eq!(rows[0].cells[3], "0");
        assert_eq!(rows[0].label("app"), Some("web"));
        assert_eq!(rows[0].owners[0].name, "web-7d9");
        assert_eq!(rows[0].created, 1_767_225_600);
        assert_eq!(rows[0].health, Health::Ok);
        assert_eq!(rows[1].health, Health::Bad);
        assert_eq!(rows[2].health, Health::Warn);
        assert!(rows[1].haystack.contains("crashloopbackoff"));
        assert!(columns(&json!({"kind": "PodList"})).is_none());
        // The typed reader gives the same rows.
        let doc: ListDoc = serde_json::from_value(t.clone()).unwrap();
        let typed = doc.into_rows(&cols, "Pod");
        assert_eq!(typed, rows);
        let p = plain_row(&json!({"metadata": {"name": "x", "namespace": "n"}}), "Thing");
        assert_eq!((p.key().as_str(), p.cells[0].as_str()), ("n/x", "x"));
    }

    #[test]
    fn cells_compare_as_people_read_them() {
        assert_eq!(compare_cells("web-2", "web-10"), Ordering::Less);
        assert_eq!(compare_cells("9", "10"), Ordering::Less);
        assert_eq!(compare_cells("1/3", "2/3"), Ordering::Less);
        assert_eq!(compare_cells("512Mi", "1Gi"), Ordering::Less);
        assert_eq!(compare_cells("250m", "1"), Ordering::Less);
        assert_eq!(compare_cells("4 (2m ago)", "12"), Ordering::Less);
        assert_eq!(compare_cells("Running", "pending"), Ordering::Greater);
    }
}
