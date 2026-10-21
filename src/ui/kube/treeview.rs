//! `:tree` in the Kubernetes view: `litek8s tree`'s picture (nodes, each
//! namespace's workloads down their controllers, services and the pods
//! they select), built by the same builder (`crate::tree`) from the
//! informers' rows the other views use: nothing is fetched for it, and it
//! changes as the watches do.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::filter::Filter;
use super::state::WatchKey;
use super::table::{Column, Health, Row as TableRow, Snapshot};
use crate::tree::{self, Item, Kind, Row};

pub struct TreeView {
    /// `None`: all namespaces.
    pub ns: Option<String>,
    /// Old ReplicaSets at zero too (`H`).
    pub all: bool,
    pub rows: Vec<Row>,
    /// Folded rows by key.
    pub folded: BTreeSet<String>,
    pub filter: Option<Filter>,
    /// Indices into `rows` on screen (folds and the filter applied), with
    /// their connectors and whether something under them is hidden.
    pub shown: Vec<usize>,
    pub prefix: Vec<String>,
    pub hidden: Vec<usize>,
    /// Index into `shown`.
    pub sel: usize,
    sel_key: Option<String>,
    pub offset: Cell<usize>,
    pub height: Cell<usize>,
    /// Lists not there yet, or refused (`pods: forbidden`).
    pub missing: Vec<String>,
    pub loading: bool,
    /// The server's Service Table has no Selector column (an older
    /// litek8s): services come as plain objects instead.
    pub service_objects: bool,
    /// A row to put the cursor on once it shows (`t` on an object).
    pub focus: Option<String>,
}

impl TreeView {
    pub fn new(ns: Option<String>) -> TreeView {
        TreeView { ns, all: false, rows: Vec::new(), folded: BTreeSet::new(), filter: None, shown: Vec::new(), prefix: Vec::new(), hidden: Vec::new(), sel: 0, sel_key: None, offset: Cell::new(0), height: Cell::new(10), missing: Vec::new(), loading: true, service_objects: false, focus: None }
    }

    /// The informers it is drawn from: the nine kinds, in its namespace
    /// (nodes are cluster-wide).
    pub fn keys(&self, ids: &HashMap<Kind, String>) -> Vec<(Kind, WatchKey)> {
        Kind::ALL
            .iter()
            .filter_map(|k| {
                let id = ids.get(k)?;
                let res = if *k == Kind::Service && self.service_objects { format!("{id}{}", super::informer::OBJECTS) } else { id.clone() };
                Some((*k, WatchKey { res, ns: if *k == Kind::Node { None } else { self.ns.clone() }, fields: String::new() }))
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&Row> {
        self.shown.get(self.sel).map(|&i| &self.rows[i])
    }

    pub fn select(&mut self, i: usize) {
        self.sel = i.min(self.shown.len().saturating_sub(1));
        self.sel_key = self.selected().map(Row::key);
    }

    /// Rebuild from the informers' snapshots.
    pub fn rebuild(&mut self, data: &HashMap<WatchKey, std::sync::Arc<Snapshot>>, ids: &HashMap<Kind, String>) {
        let mut items = Vec::new();
        self.missing.clear();
        let mut synced = 0;
        for (kind, key) in self.keys(ids) {
            // Until the plain services arrive, the Table's (without pods).
            let table_key = WatchKey { res: key.res.trim_end_matches(super::informer::OBJECTS).to_string(), ..key.clone() };
            let snap = data.get(&key).or_else(|| data.get(&table_key));
            if kind == Kind::Service && !self.service_objects && snap.is_some_and(|s| s.synced && !s.columns.is_empty() && !s.columns.iter().any(|c| c.name.eq_ignore_ascii_case("selector"))) {
                self.service_objects = true;
            }
            match snap {
                Some(s) if s.error.is_some() && s.rows.is_empty() => self.missing.push(format!("{}: {}", kind.plural(), s.error.as_deref().unwrap_or_default())),
                Some(s) => {
                    if s.synced {
                        synced += 1;
                    }
                    items.extend(s.rows.iter().map(|r| item(kind, &s.columns, r)));
                }
                None => {}
            }
        }
        self.loading = synced == 0;
        self.rows = tree::build(&items, self.all, self.ns.as_deref());
        // A group's glyph: the worst of what is under it.
        let mut worst: Option<(usize, Health)> = None;
        for i in (0..self.rows.len()).rev() {
            if self.rows[i].depth == 0 {
                if let Some((_, h)) = worst.take() {
                    self.rows[i].health = h;
                }
            } else {
                let h = self.rows[i].health;
                worst = Some(match worst {
                    Some((j, w)) if rank(w) >= rank(h) => (j, w),
                    _ => (i, h),
                });
            }
        }
        if let Some(f) = self.focus.clone() {
            if let Some(i) = self.rows.iter().position(|r| r.key() == f) {
                self.focus = None;
                self.folded.clear();
                self.refresh();
                if let Some(p) = self.shown.iter().position(|&j| j == i) {
                    self.select(p);
                    // Some air above it.
                    self.offset.set(p.saturating_sub(3));
                }
                return;
            }
        }
        self.refresh();
    }

    /// What shows: folds, then the filter (a match keeps its ancestors).
    pub fn refresh(&mut self) {
        let n = self.rows.len();
        let mut keep = vec![true; n];
        let mut hidden = vec![0usize; n];
        match self.filter.as_ref().filter(|f| !f.pattern.is_empty()) {
            Some(f) => {
                // Keep matches and their ancestors (folds are ignored).
                keep = vec![false; n];
                let mut stack: Vec<usize> = Vec::new();
                for i in 0..n {
                    while stack.last().is_some_and(|&j| self.rows[j].depth >= self.rows[i].depth) {
                        stack.pop();
                    }
                    if f.matches_text(&self.rows[i].text()) {
                        keep[i] = true;
                        for &a in &stack {
                            keep[a] = true;
                        }
                    }
                    stack.push(i);
                }
            }
            None => {
                let mut fold_at: Option<(usize, usize)> = None;
                for i in 0..n {
                    let d = self.rows[i].depth;
                    if let Some((j, fd)) = fold_at {
                        if d > fd {
                            keep[i] = false;
                            hidden[j] += 1;
                            continue;
                        }
                        fold_at = None;
                    }
                    if self.folded.contains(&self.rows[i].key()) {
                        fold_at = Some((i, d));
                    }
                }
            }
        }
        self.shown = (0..n).filter(|&i| keep[i]).collect();
        let shown_rows: Vec<Row> = self.shown.iter().map(|&i| self.rows[i].clone()).collect();
        self.prefix = tree::prefixes(&shown_rows);
        self.hidden = self.shown.iter().map(|&i| hidden[i]).collect();
        // The selection follows its row.
        let sel = self.sel_key.as_ref().and_then(|k| self.shown.iter().position(|&i| self.rows[i].key() == *k));
        match sel {
            Some(i) => self.sel = i,
            None => {
                self.sel = self.sel.min(self.shown.len().saturating_sub(1));
                self.sel_key = self.selected().map(Row::key);
            }
        }
    }

    /// Whether row `i` (into `rows`) has rows under it.
    pub fn has_children(&self, i: usize) -> bool {
        self.rows.get(i + 1).is_some_and(|r| r.depth > self.rows[i].depth)
    }

    pub fn toggle(&mut self, fold: Option<bool>) {
        let Some(&i) = self.shown.get(self.sel) else { return };
        if !self.has_children(i) {
            return;
        }
        let key = self.rows[i].key();
        let folded = self.folded.contains(&key);
        match fold.unwrap_or(!folded) {
            true => self.folded.insert(key),
            false => self.folded.remove(&key),
        };
        self.refresh();
    }

    /// `←` on an open row folds it, else goes to its parent.
    pub fn left(&mut self) {
        let Some(&i) = self.shown.get(self.sel) else { return };
        if self.has_children(i) && !self.folded.contains(&self.rows[i].key()) && self.filter.is_none() {
            return self.toggle(Some(true));
        }
        let d = self.rows[i].depth;
        if let Some(p) = (0..self.sel).rev().find(|&j| self.rows[self.shown[j]].depth < d) {
            self.select(p);
        }
    }
}

fn rank(h: Health) -> u8 {
    match h {
        Health::Done | Health::Ok => 0,
        Health::Warn => 1,
        Health::Bad => 2,
    }
}

fn cell<'a>(cols: &[Column], r: &'a TableRow, name: &str) -> &'a str {
    r.cell(cols, name).unwrap_or_default()
}

fn count(s: &str) -> i64 {
    s.split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// `1/2`: (1, 2).
fn fraction(s: &str) -> (i64, i64) {
    match s.split_once('/') {
        Some((a, b)) => (count(a.trim()), count(b.trim())),
        None => (0, 0),
    }
}

fn labels(r: &TableRow) -> BTreeMap<String, String> {
    r.labels.pointer("/metadata/labels").and_then(|l| l.as_object()).into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()
}

/// A tree item from a table row: what the server's columns say (the same
/// columns kubectl prints, `-o wide` ones included).
pub fn item(kind: Kind, cols: &[Column], r: &TableRow) -> Item {
    let mut it = Item {
        kind,
        ns: r.ns.clone(),
        name: r.name.clone(),
        uid: r.uid.clone(),
        owner: r.owners.iter().find(|o| o.controller).map(|o| o.uid.clone()),
        health: r.health,
        ..Default::default()
    };
    let c = |n: &str| cell(cols, r, n);
    match kind {
        Kind::Deployment | Kind::StatefulSet => {
            let (ready, want) = fraction(c("Ready"));
            it.detail = format!("{ready}/{want} ready");
        }
        Kind::ReplicaSet | Kind::DaemonSet => {
            let (ready, want) = (count(c("Ready")), count(c("Desired")));
            it.detail = format!("{ready}/{want} ready");
            it.health = if ready >= want { Health::Ok } else { Health::Warn };
            it.history = kind == Kind::ReplicaSet && want == 0 && count(c("Current")) == 0;
        }
        Kind::Job => {
            let (done, want) = fraction(c("Completions"));
            it.detail = format!("{done}/{want} done");
            if c("Status") == "Failed" {
                it.detail.push_str(", failed");
            }
        }
        Kind::CronJob => {
            it.detail = format!("schedule {}", c("Schedule"));
            if c("Suspend").eq_ignore_ascii_case("true") {
                it.detail.push_str(", suspended");
                it.health = Health::Done;
            }
        }
        Kind::Pod => {
            let status = c("Status");
            it.running = status == "Running";
            it.node = c("Node").to_string();
            it.labels = labels(r);
            it.detail = tree::pod_detail(status, &it.node, count(c("Restarts")));
        }
        Kind::Service => {
            let ports: Vec<&str> = c("Port(s)").split(',').filter(|p| !p.is_empty() && *p != "<none>").map(|p| p.split('/').next().unwrap_or(p)).collect();
            it.detail = tree::service_detail(c("Type"), c("Cluster-IP"), &ports.join(","));
            it.selector = c("Selector").split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        }
        Kind::Node => {
            it.detail = c("Status").to_string();
        }
    }
    it
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::kube::table;
    use serde_json::json;

    fn table_doc(cols: &[&str], rows: Vec<(serde_json::Value, Vec<&str>)>) -> (Vec<Column>, Vec<TableRow>, serde_json::Value) {
        let doc = json!({"kind": "Table", "columnDefinitions": cols.iter().map(|c| json!({"name": c, "type": "string"})).collect::<Vec<_>>(),
            "rows": rows.iter().map(|(m, cells)| json!({"cells": cells, "object": {"metadata": m}})).collect::<Vec<_>>()});
        let c = table::columns(&doc).unwrap();
        (c.clone(), Vec::new(), doc)
    }

    #[test]
    fn items_from_table_rows_match_the_cli() {
        let (cols, _, doc) = table_doc(
            &["Name", "Ready", "Status", "Restarts", "Age", "IP", "Node"],
            vec![(json!({"name": "api-1", "namespace": "web", "uid": "p1", "labels": {"app": "api"}, "ownerReferences": [{"kind": "ReplicaSet", "name": "api-abc", "uid": "r1", "controller": true}]}), vec!["api-1", "0/1", "CrashLoopBackOff", "3 (2m ago)", "", "10.0.0.1", "n1"])],
        );
        let rows = table::table_rows(&doc, &cols, "Pod");
        let it = item(Kind::Pod, &cols, &rows[0]);
        assert_eq!(it.detail, "CrashLoopBackOff on n1, 3 restarts");
        assert_eq!(it.owner.as_deref(), Some("r1"));
        assert_eq!(it.labels.get("app").map(String::as_str), Some("api"));
        assert_eq!(it.health, Health::Bad);
        let (cols, _, doc) = table_doc(&["Name", "Type", "Cluster-IP", "External-IP", "Port(s)", "Age", "Selector"], vec![(json!({"name": "api", "namespace": "web"}), vec!["api", "NodePort", "10.43.0.9", "<none>", "80:30080/TCP", "", "app=api"])]);
        let rows = table::table_rows(&doc, &cols, "Service");
        let it = item(Kind::Service, &cols, &rows[0]);
        assert_eq!(it.detail, "NodePort 10.43.0.9 80:30080");
        assert_eq!(it.selector.get("app").map(String::as_str), Some("api"));
        let (cols, _, doc) = table_doc(&["Name", "Desired", "Current", "Ready", "Age"], vec![(json!({"name": "api-old", "namespace": "web"}), vec!["api-old", "0", "0", "0", ""])]);
        let rows = table::table_rows(&doc, &cols, "ReplicaSet");
        assert!(item(Kind::ReplicaSet, &cols, &rows[0]).history);
    }
}
