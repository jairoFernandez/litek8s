//! Server-side printing: the `meta.k8s.io/v1` Table kubectl asks for with
//! `Accept: application/json;as=Table`. kubectl prints these columns as-is.

use serde_json::{Value, json};

use super::registry::ResourceType;

struct Column {
    name: &'static str,
    type_: &'static str,
    format: &'static str,
    /// Extracts the cell from the object.
    cell: fn(&Value) -> Value,
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn count(v: &Value, ptr: &str) -> Value {
    let n = v
        .pointer(ptr)
        .map(|x| match x {
            Value::Object(m) => m.len(),
            Value::Array(a) => a.len(),
            _ => 0,
        })
        .unwrap_or(0);
    json!(n)
}

fn int(v: &Value, ptr: &str) -> i64 {
    v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

fn name_cell(v: &Value) -> Value {
    json!(s(v, "/metadata/name"))
}

fn age_cell(v: &Value) -> Value {
    json!(age(&s(v, "/metadata/creationTimestamp")))
}

const NAME: Column = Column { name: "Name", type_: "string", format: "name", cell: name_cell };
const AGE: Column = Column { name: "Age", type_: "string", format: "", cell: age_cell };

fn columns_for(kind: &str) -> Vec<Column> {
    match kind {
        "ConfigMap" => vec![
            NAME,
            Column {
                name: "Data",
                type_: "integer",
                format: "",
                cell: |v| {
                    let n = count(v, "/data").as_u64().unwrap_or(0) + count(v, "/binaryData").as_u64().unwrap_or(0);
                    json!(n)
                },
            },
            AGE,
        ],
        "Secret" => vec![
            NAME,
            Column { name: "Type", type_: "string", format: "", cell: |v| json!(s(v, "/type")) },
            Column { name: "Data", type_: "integer", format: "", cell: |v| count(v, "/data") },
            AGE,
        ],
        "Namespace" => vec![
            NAME,
            Column { name: "Status", type_: "string", format: "", cell: |v| json!(s(v, "/status/phase")) },
            AGE,
        ],
        "ServiceAccount" => vec![
            NAME,
            Column { name: "Secrets", type_: "integer", format: "", cell: |v| count(v, "/secrets") },
            AGE,
        ],
        "Deployment" => vec![
            NAME,
            Column {
                name: "Ready",
                type_: "string",
                format: "",
                cell: |v| json!(format!("{}/{}", int(v, "/status/readyReplicas"), replicas(v))),
            },
            Column { name: "Up-to-date", type_: "integer", format: "", cell: |v| json!(int(v, "/status/updatedReplicas")) },
            Column { name: "Available", type_: "integer", format: "", cell: |v| json!(int(v, "/status/availableReplicas")) },
            AGE,
        ],
        "Pod" => vec![
            NAME,
            Column {
                name: "Ready",
                type_: "string",
                format: "",
                cell: |v| {
                    let statuses = v.pointer("/status/containerStatuses").and_then(Value::as_array);
                    let ready = statuses
                        .map(|s| s.iter().filter(|c| c.get("ready") == Some(&json!(true))).count())
                        .unwrap_or(0);
                    json!(format!("{ready}/{}", count(v, "/spec/containers")))
                },
            },
            Column {
                name: "Status",
                type_: "string",
                format: "",
                cell: |v| {
                    let phase = s(v, "/status/phase");
                    json!(if phase.is_empty() { "Pending".to_string() } else { phase })
                },
            },
            Column {
                name: "Restarts",
                type_: "integer",
                format: "",
                cell: |v| {
                    let n: i64 = v
                        .pointer("/status/containerStatuses")
                        .and_then(Value::as_array)
                        .map(|s| s.iter().map(|c| int(c, "/restartCount")).sum())
                        .unwrap_or(0);
                    json!(n)
                },
            },
            AGE,
        ],
        "Service" => vec![
            NAME,
            Column { name: "Type", type_: "string", format: "", cell: |v| json!(s(v, "/spec/type")) },
            Column {
                name: "Cluster-IP",
                type_: "string",
                format: "",
                cell: |v| {
                    let ip = s(v, "/spec/clusterIP");
                    json!(if ip.is_empty() { "<none>".to_string() } else { ip })
                },
            },
            Column {
                name: "Port(s)",
                type_: "string",
                format: "",
                cell: |v| {
                    let ports: Vec<String> = v
                        .pointer("/spec/ports")
                        .and_then(Value::as_array)
                        .map(|ps| {
                            ps.iter()
                                .map(|p| {
                                    let proto = p.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
                                    format!("{}/{proto}", int(p, "/port"))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!(if ports.is_empty() { "<none>".to_string() } else { ports.join(",") })
                },
            },
            AGE,
        ],
        _ => vec![NAME, AGE],
    }
}

fn replicas(v: &Value) -> i64 {
    v.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1)
}

/// Evaluate a simple dotted JSONPath (`.spec.size`, `.status.conditions[0].type`).
fn json_path(v: &Value, path: &str) -> Value {
    let mut cur = v;
    for seg in path.trim_start_matches('.').split('.').filter(|s| !s.is_empty()) {
        let (field, index) = match seg.split_once('[') {
            Some((f, rest)) => (f, rest.trim_end_matches(']').parse::<usize>().ok()),
            None => (seg, None),
        };
        let Some(next) = cur.get(field) else { return Value::Null };
        cur = match index {
            Some(i) => match next.get(i) {
                Some(x) => x,
                None => return Value::Null,
            },
            None => next,
        };
    }
    cur.clone()
}

/// Build a Table for `objects`. `include_object` adds each row's
/// PartialObjectMetadata, which kubectl needs for -A and for watch output.
pub fn build(rt: &ResourceType, objects: &[Value], rv: u64, include_object: bool) -> Value {
    let builtin = columns_for(&rt.kind);
    let mut defs: Vec<Value> = Vec::new();
    let mut extractors: Vec<Box<dyn Fn(&Value) -> Value>> = Vec::new();
    if rt.custom && !rt.printer_columns.is_empty() {
        defs.push(json!({"name": "Name", "type": "string", "format": "name", "description": "", "priority": 0}));
        extractors.push(Box::new(name_cell));
        for col in &rt.printer_columns {
            let path = col.json_path.clone();
            let is_age = col.type_ == "date";
            defs.push(json!({"name": col.name, "type": if is_age { "string" } else { col.type_.as_str() }, "format": "", "description": "", "priority": 0}));
            extractors.push(Box::new(move |v| {
                let cell = json_path(v, &path);
                if is_age {
                    json!(age(cell.as_str().unwrap_or_default()))
                } else {
                    cell
                }
            }));
        }
        if !rt.printer_columns.iter().any(|c| c.name == "Age") {
            defs.push(json!({"name": "Age", "type": "string", "format": "", "description": "", "priority": 0}));
            extractors.push(Box::new(age_cell));
        }
    } else {
        for col in builtin {
            defs.push(json!({"name": col.name, "type": col.type_, "format": col.format, "description": "", "priority": 0}));
            extractors.push(Box::new(col.cell));
        }
    }
    let rows: Vec<Value> = objects
        .iter()
        .map(|obj| {
            let cells: Vec<Value> = extractors.iter().map(|f| f(obj)).collect();
            let mut row = json!({"cells": cells});
            if include_object {
                row["object"] = json!({
                    "kind": "PartialObjectMetadata",
                    "apiVersion": "meta.k8s.io/v1",
                    "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
                });
            }
            row
        })
        .collect();
    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": rv.to_string()},
        "columnDefinitions": defs,
        "rows": rows,
    })
}

/// kubectl's age format (`45s`, `3m12s`, `5h`, `2d3h`, ...), approximately
/// matching k8s.io/apimachinery's HumanDuration.
pub fn age(timestamp: &str) -> String {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return "<unknown>".into();
    };
    let secs = (chrono::Utc::now() - created.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86400);
    match secs {
        s if s < 120 => format!("{s}s"),
        _ if m < 10 => format!("{m}m{}s", secs % 60),
        _ if h < 3 => format!("{m}m"),
        _ if h < 8 => format!("{h}h{}m", m % 60),
        _ if h < 48 => format!("{h}h"),
        _ if d < 8 => format!("{d}d{}h", h % 24),
        _ if d < 365 * 2 => format!("{d}d"),
        _ => format!("{}y", d / 365),
    }
}
