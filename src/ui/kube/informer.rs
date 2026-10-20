//! Informers: one list (as a Table, 500 rows a page) and then a watch from
//! its resourceVersion per resource type, namespace and field selector,
//! keeping compact rows; bursts of changes go to the UI as one snapshot at
//! most every 100 ms. The containers "type" watches one pod.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hyper::Method;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::resources::Resource;
use super::state::{CONTAINERS, KMsg, WatchKey};
use super::table::{self, Column, Health, Row, Snapshot, TABLE_ACCEPT};
use crate::ui::app::Msg;
use crate::ui::client::{Endpoint, Lines, enc};

pub fn spawn(ep: Endpoint, res: Resource, key: WatchKey, tx: UnboundedSender<Msg>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(ep, res, key, tx))
}

struct State {
    columns: Arc<Vec<Column>>,
    rows: BTreeMap<String, Arc<Row>>,
    rv: String,
}

fn query(key: &WatchKey, extra: &str) -> String {
    let mut q = format!("?{extra}");
    if !key.fields.is_empty() {
        q.push_str(&format!("&fieldSelector={}", enc(&key.fields)));
    }
    q
}

fn send(tx: &UnboundedSender<Msg>, key: &WatchKey, st: &State, error: Option<String>) -> bool {
    let snap = Snapshot { columns: st.columns.clone(), rows: st.rows.values().cloned().collect(), synced: true, error };
    tx.send(Msg::Kube(KMsg::Data(key.clone(), Arc::new(snap)))).is_ok()
}

fn error_snapshot(tx: &UnboundedSender<Msg>, key: &WatchKey, e: String) -> bool {
    tx.send(Msg::Kube(KMsg::Data(key.clone(), Arc::new(Snapshot { columns: Arc::new(Vec::new()), rows: Vec::new(), synced: false, error: Some(e) })))).is_ok()
}

/// Rows of a list or watch object: a Table's rows, or a plain object.
fn rows_of(obj: &Value, columns: &[Column], kind: &str, containers: bool) -> Vec<Row> {
    if containers {
        return container_rows(obj).1;
    }
    if obj["kind"].as_str() == Some("Table") { table::table_rows(obj, columns, kind) } else { vec![table::plain_row(obj, kind)] }
}

async fn list(ep: &Endpoint, res: &Resource, key: &WatchKey) -> anyhow::Result<State> {
    let containers = res.plural == CONTAINERS;
    let base = if containers { format!("/api/v1/namespaces/{}/pods", key.ns.as_deref().unwrap_or("default")) } else { res.list_path(key.ns.as_deref()) };
    let accept = if containers { "application/json" } else { TABLE_ACCEPT };
    let mut st = State { columns: Arc::new(Vec::new()), rows: BTreeMap::new(), rv: String::new() };
    let mut cont = String::new();
    loop {
        let mut q = query(key, "limit=500&includeObject=Metadata");
        if !cont.is_empty() {
            q.push_str(&format!("&continue={}", enc(&cont)));
        }
        let doc = ep.json_accept(&format!("{base}{q}"), accept).await?;
        if containers {
            let mut cols = Vec::new();
            for p in doc["items"].as_array().into_iter().flatten() {
                let (c, rows) = container_rows(p);
                cols = c;
                for r in rows {
                    st.rows.insert(r.key(), Arc::new(r));
                }
            }
            st.columns = Arc::new(if cols.is_empty() { container_rows(&Value::Null).0 } else { cols });
        } else if let Some(cols) = table::columns(&doc) {
            if !cols.is_empty() {
                st.columns = Arc::new(cols);
            }
            for r in table::table_rows(&doc, &st.columns, &res.kind) {
                st.rows.insert(r.key(), Arc::new(r));
            }
        } else {
            st.columns = Arc::new(table::plain_columns());
            for o in doc["items"].as_array().into_iter().flatten() {
                let r = table::plain_row(o, &res.kind);
                st.rows.insert(r.key(), Arc::new(r));
            }
        }
        st.rv = doc.pointer("/metadata/resourceVersion").and_then(Value::as_str).unwrap_or_default().to_string();
        cont = doc.pointer("/metadata/continue").and_then(Value::as_str).unwrap_or_default().to_string();
        if cont.is_empty() {
            return Ok(st);
        }
    }
}

async fn run(ep: Endpoint, res: Resource, key: WatchKey, tx: UnboundedSender<Msg>) {
    let containers = res.plural == CONTAINERS;
    let mut backoff = Duration::from_secs(1);
    let mut last: Option<State> = None;
    loop {
        let mut st = match list(&ep, &res, &key).await {
            Ok(st) => st,
            Err(e) => {
                let msg = format!("{e:#}");
                let ok = match &last {
                    Some(st) => send(&tx, &key, st, Some(msg)),
                    None => error_snapshot(&tx, &key, msg),
                };
                if !ok {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        if !send(&tx, &key, &st, None) {
            return;
        }
        // Watch from the list's version until the server ends it.
        loop {
            let base = if containers { format!("/api/v1/namespaces/{}/pods", key.ns.as_deref().unwrap_or("default")) } else { res.list_path(key.ns.as_deref()) };
            let path = format!("{base}{}", query(&key, &format!("watch=1&allowWatchBookmarks=true&includeObject=Metadata&resourceVersion={}", enc(&st.rv))));
            let accept = if containers { "application/json" } else { TABLE_ACCEPT };
            let resp = match ep.send_with(Method::GET, &path, &[("Accept", accept)], None, Vec::new()).await {
                Ok(r) if r.status().is_success() => r,
                _ => break,
            };
            let mut lines = Lines::new(resp.into_body());
            let mut dirty = false;
            let mut gone = false;
            loop {
                let next = if dirty { tokio::time::timeout(Duration::from_millis(100), lines.next()).await } else { Ok(lines.next().await) };
                let line = match next {
                    Err(_) => {
                        dirty = false;
                        if !send(&tx, &key, &st, None) {
                            return;
                        }
                        continue;
                    }
                    Ok(None) => break,
                    Ok(Some(l)) => l,
                };
                let Ok(ev) = serde_json::from_slice::<Value>(&line) else { continue };
                let obj = &ev["object"];
                if let Some(rv) = obj.pointer("/metadata/resourceVersion").and_then(Value::as_str) {
                    st.rv = rv.to_string();
                }
                match ev["type"].as_str() {
                    Some("ADDED") | Some("MODIFIED") => {
                        if let Some(cols) = table::columns(obj).filter(|c| !c.is_empty()) {
                            if *st.columns != cols {
                                st.columns = Arc::new(cols);
                            }
                        }
                        if containers {
                            // The pod's containers replace what was there.
                            st.rows.clear();
                        }
                        for r in rows_of(obj, &st.columns, &res.kind, containers) {
                            st.rows.insert(r.key(), Arc::new(r));
                        }
                        dirty = true;
                    }
                    Some("DELETED") => {
                        if containers {
                            st.rows.clear();
                        }
                        for r in rows_of(obj, &st.columns, &res.kind, containers) {
                            st.rows.remove(&r.key());
                        }
                        dirty = true;
                    }
                    Some("ERROR") => {
                        gone = obj["code"].as_i64() == Some(410);
                        break;
                    }
                    _ => {}
                }
            }
            if dirty && !send(&tx, &key, &st, None) {
                return;
            }
            if gone {
                break;
            }
            if tx.is_closed() {
                return;
            }
        }
        last = Some(st);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A pod's containers as rows of the containers view.
pub fn container_rows(pod: &Value) -> (Vec<Column>, Vec<Row>) {
    let cols: Vec<Column> = ["Name", "Image", "Ready", "State", "Init", "Restarts", "Ports", "Age"].iter().map(|n| Column::new(n, if *n == "Ports" { 1 } else { 0 })).collect();
    let ns = pod.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let mut rows = Vec::new();
    for (list, statuses, init) in [("initContainers", "initContainerStatuses", true), ("containers", "containerStatuses", false), ("ephemeralContainers", "ephemeralContainerStatuses", false)] {
        for c in pod.pointer(&format!("/spec/{list}")).and_then(Value::as_array).into_iter().flatten() {
            let name = c["name"].as_str().unwrap_or_default();
            let st = pod.pointer(&format!("/status/{statuses}")).and_then(Value::as_array).and_then(|a| a.iter().find(|s| s["name"].as_str() == Some(name))).cloned().unwrap_or(Value::Null);
            let (state, started) = match st["state"].as_object().and_then(|m| m.iter().next()) {
                Some((k, v)) => {
                    let reason = v["reason"].as_str().map(|r| format!(" ({r})")).unwrap_or_default();
                    (format!("{}{reason}", super::describe::title(k)), v["startedAt"].as_str().unwrap_or_default().to_string())
                }
                None => ("Waiting".to_string(), String::new()),
            };
            let ready = st["ready"].as_bool() == Some(true);
            let health = if state.starts_with("Running") {
                if ready { Health::Ok } else { Health::Warn }
            } else if state.starts_with("Terminated (Completed)") {
                Health::Done
            } else if state.starts_with("Waiting (ContainerCreating)") || state.starts_with("Waiting (PodInitializing)") || state == "Waiting" {
                Health::Warn
            } else {
                Health::Bad
            };
            let ports: Vec<String> = c["ports"].as_array().into_iter().flatten().map(|p| format!("{}{}", p["name"].as_str().map(|n| format!("{n}:")).unwrap_or_default(), p["containerPort"])).collect();
            let cells = vec![
                name.to_string(),
                c["image"].as_str().unwrap_or_default().to_string(),
                ready.to_string(),
                state,
                init.to_string(),
                st["restartCount"].as_i64().unwrap_or(0).to_string(),
                ports.join(","),
                String::new(),
            ];
            let mut hay = name.to_lowercase();
            for c in &cells {
                hay.push(' ');
                hay.push_str(&c.to_lowercase());
            }
            rows.push(Row { ns: ns.to_string(), name: name.to_string(), uid: String::new(), cells, created: table::timestamp(Some(&started)), deleting: false, labels: Value::Null, owners: Vec::new(), health, haystack: hay });
        }
    }
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn containers_of_a_pod() {
        let pod = json!({"metadata": {"name": "p", "namespace": "n"}, "spec": {"initContainers": [{"name": "setup", "image": "busybox"}], "containers": [{"name": "app", "image": "nginx", "ports": [{"name": "http", "containerPort": 80}]}]},
            "status": {"initContainerStatuses": [{"name": "setup", "state": {"terminated": {"reason": "Completed"}}}], "containerStatuses": [{"name": "app", "ready": true, "restartCount": 1, "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}}}]}});
        let (cols, rows) = container_rows(&pod);
        assert_eq!(cols.len(), 8);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cells[3], "Terminated (Completed)");
        assert_eq!(rows[0].health, Health::Done);
        assert_eq!(rows[1].cells[3], "Running");
        assert_eq!(rows[1].cells[6], "http:80");
        assert_eq!(rows[1].key(), "n/app");
        assert_eq!(rows[1].created, 1_767_225_600);
    }
}
