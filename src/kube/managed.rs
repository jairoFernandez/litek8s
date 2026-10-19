//! Field management: `metadata.managedFields` and server-side apply, after
//! sigs.k8s.io/structured-merge-diff.
//!
//! Every write records which manager owns which fields (`fieldsV1`). An
//! update (create, update, patch) gives its manager the fields it added or
//! changed and takes them from everyone else. An apply merges the applied
//! configuration into the object following the schema's list and map
//! types (`x-kubernetes-list-type` atomic/set/map with
//! `x-kubernetes-list-map-keys`, `x-kubernetes-map-type`, and the merge
//! keys of the built-in types), reports conflicts with other managers that
//! own a field it changes (409, unless forced, which takes the field
//! over), and removes fields the manager applied before but no longer
//! applies, unless someone else owns them.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Field sets

/// One step of a field path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pe {
    Field(String),
    /// A list-map item, by its key fields (canonical JSON object).
    Key(String),
    /// A list-set item (canonical JSON value).
    Value(String),
    Index(usize),
}

impl Pe {
    fn encoded(&self) -> String {
        match self {
            Pe::Field(f) => format!("f:{f}"),
            Pe::Key(k) => format!("k:{k}"),
            Pe::Value(v) => format!("v:{v}"),
            Pe::Index(i) => format!("i:{i}"),
        }
    }

    fn decode(s: &str) -> Option<Pe> {
        let (kind, rest) = s.split_once(':')?;
        Some(match kind {
            "f" => Pe::Field(rest.to_string()),
            "k" => Pe::Key(canonical(&serde_json::from_str(rest).ok()?)),
            "v" => Pe::Value(canonical(&serde_json::from_str(rest).ok()?)),
            "i" => Pe::Index(rest.parse().ok()?),
            _ => return None,
        })
    }

    /// How structured-merge-diff prints a path element.
    fn shown(&self) -> String {
        match self {
            Pe::Field(f) => format!(".{f}"),
            Pe::Key(k) => {
                let v: Value = serde_json::from_str(k).unwrap_or(Value::Null);
                let parts: Vec<String> = v.as_object().into_iter().flatten().map(|(k, v)| format!("{k}={v}")).collect();
                format!("[{}]", parts.join(","))
            }
            Pe::Value(v) => format!("[={v}]"),
            Pe::Index(i) => format!("[{i}]"),
        }
    }
}

/// Canonical JSON (object keys sorted, which serde_json's map does).
fn canonical(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

pub fn path_string(path: &[Pe]) -> String {
    path.iter().map(Pe::shown).collect()
}

/// A set of field paths, as a tree: `members` are paths that end here,
/// `children` continue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Set {
    members: BTreeSet<Pe>,
    children: BTreeMap<Pe, Set>,
}

impl Set {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.children.values().all(Set::is_empty)
    }

    pub fn insert(&mut self, path: &[Pe]) {
        match path {
            [] => {}
            [last] => {
                self.members.insert(last.clone());
            }
            [first, rest @ ..] => self.children.entry(first.clone()).or_default().insert(rest),
        }
    }

    pub fn has(&self, path: &[Pe]) -> bool {
        match path {
            [] => false,
            [last] => self.members.contains(last),
            [first, rest @ ..] => self.children.get(first).is_some_and(|c| c.has(rest)),
        }
    }

    pub fn union(&self, o: &Set) -> Set {
        let mut out = self.clone();
        out.members.extend(o.members.iter().cloned());
        for (pe, c) in &o.children {
            let merged = match out.children.get(pe) {
                Some(mine) => mine.union(c),
                None => c.clone(),
            };
            out.children.insert(pe.clone(), merged);
        }
        out
    }

    pub fn difference(&self, o: &Set) -> Set {
        let mut out = Set { members: self.members.difference(&o.members).cloned().collect(), children: BTreeMap::new() };
        for (pe, c) in &self.children {
            let d = match o.children.get(pe) {
                Some(oc) => c.difference(oc),
                None => c.clone(),
            };
            if !d.is_empty() {
                out.children.insert(pe.clone(), d);
            }
        }
        out
    }

    pub fn intersection(&self, o: &Set) -> Set {
        let mut out = Set { members: self.members.intersection(&o.members).cloned().collect(), children: BTreeMap::new() };
        for (pe, c) in &self.children {
            if let Some(oc) = o.children.get(pe) {
                let i = c.intersection(oc);
                if !i.is_empty() {
                    out.children.insert(pe.clone(), i);
                }
            }
        }
        out
    }

    /// Every path in the set.
    pub fn paths(&self) -> Vec<Vec<Pe>> {
        let mut out = Vec::new();
        self.collect(&mut Vec::new(), &mut out);
        out.sort();
        out
    }

    fn collect(&self, prefix: &mut Vec<Pe>, out: &mut Vec<Vec<Pe>>) {
        for m in &self.members {
            prefix.push(m.clone());
            out.push(prefix.clone());
            prefix.pop();
        }
        for (pe, c) in &self.children {
            prefix.push(pe.clone());
            c.collect(prefix, out);
            prefix.pop();
        }
    }

    fn remove_subtree(&mut self, pe: &Pe) {
        self.members.remove(pe);
        self.children.remove(pe);
    }

    /// The `fieldsV1` encoding.
    pub fn to_fields_v1(&self) -> Value {
        let mut m = Map::new();
        let mut keys: BTreeSet<&Pe> = self.members.iter().collect();
        keys.extend(self.children.keys());
        for pe in keys {
            let mut inner = match self.children.get(pe) {
                Some(c) => match c.to_fields_v1() {
                    Value::Object(o) => o,
                    _ => Map::new(),
                },
                None => Map::new(),
            };
            if self.children.contains_key(pe) && self.members.contains(pe) {
                let mut with_dot = Map::new();
                with_dot.insert(".".into(), json!({}));
                with_dot.extend(inner);
                inner = with_dot;
            }
            m.insert(pe.encoded(), Value::Object(inner));
        }
        Value::Object(m)
    }

    pub fn from_fields_v1(v: &Value) -> Option<Set> {
        let mut s = Set::default();
        for (k, child) in v.as_object()? {
            if k == "." {
                continue;
            }
            let pe = Pe::decode(k)?;
            let obj = child.as_object()?;
            if obj.is_empty() {
                s.members.insert(pe);
                continue;
            }
            if obj.contains_key(".") {
                s.members.insert(pe.clone());
            }
            let c = Set::from_fields_v1(child)?;
            if !c.is_empty() {
                s.children.insert(pe, c);
            }
        }
        Some(s)
    }
}

// ---------------------------------------------------------------------------
// Schemas

/// How a node merges and is owned.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Scalar,
    /// An object with named fields.
    Struct,
    /// An object with arbitrary keys.
    Map,
    /// Replaced as a whole and owned as one field.
    Atomic,
    Set,
    /// A list of objects identified by these key fields (with defaults).
    Keyed(Vec<(String, Option<Value>)>),
}

/// Where a node is in a schema: a CRD's OpenAPI schema, ObjectMeta, a
/// built-in type (described by field names), or nothing (deduced from the
/// value: maps are granular, lists atomic).
#[derive(Debug, Clone, Copy)]
pub enum Ty<'a> {
    Crd(&'a Value),
    CrdRoot(&'a Value),
    Meta,
    Builtin(&'a str),
    BuiltinRoot,
    Deduced,
}

/// Object fields of the built-in types that are maps (for ownership they
/// behave like structs; only `.` markers in Update entries differ).
const BUILTIN_MAPS: &[&str] = &[
    "labels", "annotations", "data", "binaryData", "stringData", "nodeSelector", "matchLabels", "limits", "requests", "capacity",
    "allocatable", "hard", "used", "parameters", "volumeAttributes", "attributes",
];

/// List-map keys of the built-in types, by field name.
fn builtin_keys(field: &str, items: &[Value]) -> Option<Vec<(String, Option<Value>)>> {
    let key = |k: &str| (k.to_string(), None);
    Some(match field {
        "containers" | "initContainers" | "ephemeralContainers" | "volumes" | "env" | "imagePullSecrets" | "webhooks" | "envFrom"
        | "resourceClaims" => {
            if field == "envFrom" {
                return None;
            }
            vec![key("name")]
        }
        "volumeMounts" => vec![key("mountPath")],
        "volumeDevices" => vec![key("devicePath")],
        "ownerReferences" => vec![key("uid")],
        "conditions" => vec![key("type")],
        "hostAliases" => vec![key("ip")],
        "topologySpreadConstraints" => vec![key("topologyKey"), key("whenUnsatisfiable")],
        "ports" => {
            let container = items.iter().any(|i| i.get("containerPort").is_some());
            vec![(if container { "containerPort" } else { "port" }.to_string(), None), ("protocol".to_string(), Some(json!("TCP")))]
        }
        _ => return None,
    })
}

fn flag(s: &Value, k: &str) -> bool {
    s.get(k).and_then(Value::as_bool) == Some(true)
}

impl<'a> Ty<'a> {
    pub fn shape(&self, v: &Value) -> Shape {
        let deduced = |v: &Value| match v {
            Value::Object(_) => Shape::Map,
            Value::Array(_) => Shape::Atomic,
            _ => Shape::Scalar,
        };
        match self {
            Ty::Deduced => deduced(v),
            Ty::BuiltinRoot => deduced(v),
            Ty::Meta => match v {
                Value::Object(_) => Shape::Struct,
                Value::Array(_) => Shape::Atomic,
                _ => Shape::Scalar,
            },
            Ty::Builtin(field) => match v {
                Value::Object(_) => {
                    if BUILTIN_MAPS.contains(field) {
                        Shape::Map
                    } else {
                        Shape::Struct
                    }
                }
                Value::Array(items) => {
                    if *field == "finalizers" {
                        Shape::Set
                    } else {
                        builtin_keys(field, items).map(Shape::Keyed).unwrap_or(Shape::Atomic)
                    }
                }
                _ => Shape::Scalar,
            },
            Ty::Crd(s) | Ty::CrdRoot(s) => {
                if flag(s, "x-kubernetes-int-or-string") {
                    return Shape::Scalar;
                }
                match s.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "object" => {
                        if s.get("x-kubernetes-map-type").and_then(Value::as_str) == Some("atomic") {
                            Shape::Atomic
                        } else if s.get("properties").is_some_and(|p| p.as_object().is_some_and(|p| !p.is_empty())) {
                            Shape::Struct
                        } else if s.get("additionalProperties").is_some_and(|a| a.is_object() || a == &json!(true)) || flag(s, "x-kubernetes-preserve-unknown-fields") {
                            Shape::Map
                        } else {
                            Shape::Struct
                        }
                    }
                    "array" => match s.get("x-kubernetes-list-type").and_then(Value::as_str) {
                        Some("set") => Shape::Set,
                        Some("map") => {
                            let keys: Vec<(String, Option<Value>)> = s
                                .get("x-kubernetes-list-map-keys")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(|k| (k.to_string(), s.pointer(&format!("/items/properties/{k}/default")).cloned()))
                                .collect();
                            Shape::Keyed(keys)
                        }
                        _ => Shape::Atomic,
                    },
                    "" if flag(s, "x-kubernetes-preserve-unknown-fields") => deduced(v),
                    "" => deduced(v),
                    _ => Shape::Scalar,
                }
            }
        }
    }

    pub fn field(&self, name: &str) -> Ty<'a> {
        match self {
            Ty::Deduced => Ty::Deduced,
            Ty::BuiltinRoot => match name {
                "metadata" => Ty::Meta,
                _ => Ty::Builtin(builtin_name(name)),
            },
            Ty::Builtin(_) => Ty::Builtin(builtin_name(name)),
            Ty::Meta => match name {
                "labels" | "annotations" => Ty::Deduced,
                "finalizers" | "ownerReferences" | "managedFields" => Ty::Builtin(builtin_name(name)),
                _ => Ty::Deduced,
            },
            Ty::CrdRoot(s) if name == "metadata" => {
                let _ = s;
                Ty::Meta
            }
            Ty::Crd(s) | Ty::CrdRoot(s) => {
                if let Some(p) = s.get("properties").and_then(|p| p.get(name)) {
                    return if flag(p, "x-kubernetes-embedded-resource") { Ty::CrdRoot(p) } else { Ty::Crd(p) };
                }
                match s.get("additionalProperties").filter(|a| a.is_object()) {
                    Some(a) => {
                        if flag(a, "x-kubernetes-embedded-resource") {
                            Ty::CrdRoot(a)
                        } else {
                            Ty::Crd(a)
                        }
                    }
                    None => Ty::Deduced,
                }
            }
        }
    }

    pub fn items(&self) -> Ty<'a> {
        match self {
            Ty::Crd(s) | Ty::CrdRoot(s) => match s.get("items").filter(|i| i.is_object()) {
                Some(i) if flag(i, "x-kubernetes-embedded-resource") => Ty::CrdRoot(i),
                Some(i) => Ty::Crd(i),
                None => Ty::Deduced,
            },
            Ty::Builtin(_) => Ty::Builtin("<item>"),
            _ => Ty::Deduced,
        }
    }
}

/// Field names live as long as the schema; built-in names are matched
/// against a fixed vocabulary.
fn builtin_name(name: &str) -> &'static str {
    const NAMES: &[&str] = &[
        "labels", "annotations", "data", "binaryData", "stringData", "nodeSelector", "matchLabels", "limits", "requests", "capacity",
        "allocatable", "hard", "used", "parameters", "volumeAttributes", "attributes", "containers", "initContainers",
        "ephemeralContainers", "volumes", "env", "imagePullSecrets", "webhooks", "envFrom", "resourceClaims", "volumeMounts",
        "volumeDevices", "ownerReferences", "conditions", "hostAliases", "topologySpreadConstraints", "ports", "finalizers",
        "managedFields",
    ];
    NAMES.iter().find(|n| **n == name).copied().unwrap_or("<field>")
}

fn item_pe(item: &Value, shape: &Shape, index: usize) -> Result<Pe, String> {
    match shape {
        Shape::Set => Ok(Pe::Value(canonical(item))),
        Shape::Keyed(keys) => {
            let mut key = Map::new();
            for (k, default) in keys {
                match item.get(k).filter(|v| !v.is_null()).or(default.as_ref()) {
                    Some(v) => {
                        key.insert(k.clone(), v.clone());
                    }
                    None => {
                        return Err(format!("associative list with keys has an element that omits key field {k:?} (and doesn't have default value)"));
                    }
                }
            }
            Ok(Pe::Key(canonical(&Value::Object(key))))
        }
        _ => Ok(Pe::Index(index)),
    }
}

// ---------------------------------------------------------------------------
// Walks

/// The fields an applied configuration sets (structured-merge-diff's
/// `ToFieldSet`): leaves, list items, atomic lists and maps.
pub fn to_set(v: &Value, ty: Ty) -> Result<Set, String> {
    let mut out = Set::default();
    to_set_at(v, ty, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn to_set_at(v: &Value, ty: Ty, path: &mut Vec<Pe>, out: &mut Set) -> Result<(), String> {
    let shape = ty.shape(v);
    match (&shape, v) {
        (Shape::Struct | Shape::Map, Value::Object(m)) => {
            for (k, child) in m {
                path.push(Pe::Field(k.clone()));
                to_set_at(child, ty.field(k), path, out)?;
                path.pop();
            }
        }
        (Shape::Set | Shape::Keyed(_), Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                path.push(item_pe(item, &shape, i)?);
                to_set_at(item, ty.items(), path, out)?;
                out.insert(path);
                path.pop();
            }
        }
        _ => out.insert(path),
    }
    Ok(())
}

/// Merge an applied configuration into the live object.
pub fn merge(lhs: Option<&Value>, rhs: Option<&Value>, ty: Ty) -> Result<Option<Value>, String> {
    let (l, r) = match (lhs, rhs) {
        (l, None) => return Ok(l.cloned()),
        (None, Some(r)) => return Ok(Some(r.clone())),
        (Some(l), Some(r)) => (l, r),
    };
    let shape = ty.shape(r);
    Ok(Some(match (&shape, l, r) {
        (Shape::Struct | Shape::Map, Value::Object(lm), Value::Object(rm)) => {
            let mut out = lm.clone();
            for (k, rv) in rm {
                match merge(lm.get(k), Some(rv), ty.field(k))? {
                    Some(v) => {
                        out.insert(k.clone(), v);
                    }
                    None => {
                        out.remove(k);
                    }
                }
            }
            Value::Object(out)
        }
        (Shape::Set | Shape::Keyed(_), Value::Array(la), Value::Array(ra)) => Value::Array(merge_list(la, ra, &shape, ty.items())?),
        _ => r.clone(),
    }))
}

/// structured-merge-diff's list merge: items of both sides, the applied
/// order for shared items, live-only items kept where they were.
fn merge_list(lhs: &[Value], rhs: &[Value], shape: &Shape, item_ty: Ty) -> Result<Vec<Value>, String> {
    let rpes: Vec<Pe> = rhs.iter().enumerate().map(|(i, v)| item_pe(v, shape, i)).collect::<Result<_, _>>()?;
    let lpes: Vec<Pe> = lhs.iter().enumerate().map(|(i, v)| item_pe(v, shape, i)).collect::<Result<_, _>>()?;
    let lmap: BTreeMap<&Pe, &Value> = lpes.iter().zip(lhs).collect();
    let rmap: BTreeMap<&Pe, &Value> = rpes.iter().zip(rhs).collect();
    let mut shared: std::collections::VecDeque<&Pe> = rpes.iter().filter(|pe| lmap.contains_key(pe)).collect();
    let mut next_shared = shared.pop_front();
    let mut merged_rhs: BTreeSet<&Pe> = BTreeSet::new();
    let mut out = Vec::new();
    let (mut li, mut ri) = (0, 0);
    while li < lpes.len() || ri < rpes.len() {
        if li < lpes.len() && ri < rpes.len() {
            let pe = &lpes[li];
            if *pe == rpes[ri] {
                merged_rhs.insert(pe);
                if let Some(v) = merge(lmap.get(pe).copied(), rmap.get(pe).copied(), item_ty)? {
                    out.push(v);
                }
                li += 1;
                ri += 1;
                next_shared = shared.pop_front();
                continue;
            }
            if rmap.contains_key(pe) && next_shared.is_some_and(|n| n != pe) {
                li += 1;
                continue;
            }
        }
        if li < lpes.len() {
            let pe = &lpes[li];
            if !rmap.contains_key(pe) {
                out.push(lhs[li].clone());
                li += 1;
                continue;
            } else if merged_rhs.contains(pe) {
                li += 1;
                continue;
            }
        }
        if ri < rpes.len() {
            let pe = &rpes[ri];
            merged_rhs.insert(pe);
            if let Some(v) = merge(lmap.get(pe).copied(), rmap.get(pe).copied(), item_ty)? {
                out.push(v);
            }
            ri += 1;
            if next_shared.is_some_and(|n| n == pe) {
                next_shared = shared.pop_front();
            }
        }
    }
    Ok(out)
}

/// Added, modified and removed paths between two versions of an object.
#[derive(Default, Debug)]
pub struct Comparison {
    pub added: Set,
    pub modified: Set,
    pub removed: Set,
}

pub fn compare(lhs: &Value, rhs: &Value, ty: Ty) -> Comparison {
    let mut c = Comparison::default();
    compare_at(Some(lhs), Some(rhs), ty, &mut Vec::new(), &mut c);
    c
}

fn empty_container(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::Object(m)) => m.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

fn compare_at(lhs: Option<&Value>, rhs: Option<&Value>, ty: Ty, path: &mut Vec<Pe>, c: &mut Comparison) {
    let Some(sample) = rhs.or(lhs) else { return };
    let shape = ty.shape(sample);
    let container = matches!(shape, Shape::Struct | Shape::Map | Shape::Set | Shape::Keyed(_));
    let same_kind = match (lhs, rhs) {
        (Some(l), Some(r)) => std::mem::discriminant(l) == std::mem::discriminant(r),
        _ => true,
    };
    let leaf = !container || !same_kind || (empty_container(lhs) && empty_container(rhs) && matches!(sample, Value::Object(_) | Value::Array(_) | Value::Null));
    if leaf {
        match (lhs, rhs) {
            (None, Some(_)) => c.added.insert(path),
            (Some(_), None) => c.removed.insert(path),
            (Some(l), Some(r)) if l != r => c.modified.insert(path),
            _ => {}
        }
        return;
    }
    match (&shape, sample) {
        (Shape::Struct | Shape::Map, _) => {
            let empty = Map::new();
            let lm = lhs.and_then(Value::as_object).unwrap_or(&empty);
            let rm = rhs.and_then(Value::as_object).unwrap_or(&empty);
            let keys: BTreeSet<&String> = lm.keys().chain(rm.keys()).collect();
            for k in keys {
                path.push(Pe::Field(k.clone()));
                compare_at(lm.get(k), rm.get(k), ty.field(k), path, c);
                path.pop();
            }
        }
        _ => {
            let empty = Vec::new();
            let la = lhs.and_then(Value::as_array).unwrap_or(&empty);
            let ra = rhs.and_then(Value::as_array).unwrap_or(&empty);
            let index = |a: &Vec<Value>| -> BTreeMap<Pe, Value> {
                a.iter().enumerate().filter_map(|(i, v)| item_pe(v, &shape, i).ok().map(|pe| (pe, v.clone()))).collect()
            };
            let (lmap, rmap) = (index(la), index(ra));
            let keys: BTreeSet<&Pe> = lmap.keys().chain(rmap.keys()).collect();
            for pe in keys {
                path.push(pe.clone());
                compare_at(lmap.get(pe), rmap.get(pe), ty.items(), path, c);
                path.pop();
            }
        }
    }
    match (lhs, rhs) {
        (None, Some(_)) => c.added.insert(path),
        (Some(_), None) => c.removed.insert(path),
        _ => {}
    }
}

/// Paths the API server owns: never pruned from an object.
fn protected(path: &[Pe]) -> bool {
    match path {
        [Pe::Field(f)] => matches!(f.as_str(), "apiVersion" | "kind" | "metadata"),
        [Pe::Field(m), Pe::Field(f)] if m == "metadata" => matches!(
            f.as_str(),
            "name" | "namespace" | "uid" | "resourceVersion" | "generation" | "creationTimestamp" | "managedFields" | "deletionTimestamp"
                | "deletionGracePeriodSeconds" | "generateName" | "selfLink"
        ),
        _ => false,
    }
}

/// The object without the paths in `set`.
pub fn remove_items(v: &Value, set: &Set, ty: Ty) -> Value {
    remove_at(v, set, ty, &mut Vec::new())
}

fn remove_at(v: &Value, set: &Set, ty: Ty, path: &mut Vec<Pe>) -> Value {
    let shape = ty.shape(v);
    match (&shape, v) {
        (Shape::Struct | Shape::Map, Value::Object(m)) => {
            let mut out = Map::new();
            for (k, child) in m {
                let pe = Pe::Field(k.clone());
                path.push(pe.clone());
                if set.members.contains(&pe) && !protected(path) {
                    path.pop();
                    continue;
                }
                let child = match set.children.get(&pe) {
                    Some(sub) => remove_at(child, sub, ty.field(k), path),
                    None => child.clone(),
                };
                path.pop();
                out.insert(k.clone(), child);
            }
            Value::Object(out)
        }
        (Shape::Set | Shape::Keyed(_), Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let Ok(pe) = item_pe(item, &shape, i) else {
                    out.push(item.clone());
                    continue;
                };
                if set.members.contains(&pe) {
                    continue;
                }
                path.push(pe.clone());
                out.push(match set.children.get(&pe) {
                    Some(sub) => remove_at(item, sub, ty.items(), path),
                    None => item.clone(),
                });
                path.pop();
            }
            Value::Array(out)
        }
        _ => v.clone(),
    }
}

/// structured-merge-diff's `EnsureNamedFieldsAreMembers`: struct fields
/// with owned children count as owned themselves.
fn ensure_named(set: &Set, v: Option<&Value>, ty: Ty) -> Set {
    let mut out = set.clone();
    let shape = v.map(|v| ty.shape(v)).unwrap_or(Shape::Struct);
    for (pe, child) in &set.children {
        let (child_v, child_ty) = match pe {
            Pe::Field(f) => (v.and_then(|v| v.get(f)), ty.field(f)),
            _ => {
                let item = v.and_then(Value::as_array).and_then(|items| {
                    items.iter().enumerate().find(|(i, it)| item_pe(it, &shape, *i).ok().as_ref() == Some(pe)).map(|(_, it)| it)
                });
                (item, ty.items())
            }
        };
        if matches!(pe, Pe::Field(_)) && shape == Shape::Struct {
            out.members.insert(pe.clone());
        }
        out.children.insert(pe.clone(), ensure_named(child, child_v, child_ty));
    }
    out
}

// ---------------------------------------------------------------------------
// Managers

#[derive(Debug, Clone)]
pub struct Entry {
    pub manager: String,
    pub operation: String,
    pub api_version: String,
    pub time: Option<String>,
    pub subresource: String,
    pub set: Set,
}

impl Entry {
    fn same_identity(&self, o: &Entry) -> bool {
        self.manager == o.manager
            && self.operation == o.operation
            && self.subresource == o.subresource
            && (self.operation == "Apply" || self.api_version == o.api_version)
    }

    /// How conflicts name a manager.
    fn shown(&self) -> String {
        let mut s = format!("{:?}", self.manager);
        if !self.subresource.is_empty() {
            s = format!("{s} with subresource {:?}", self.subresource);
        }
        if self.operation == "Update" {
            s = match &self.time {
                Some(t) => format!("{s} using {} at {t}", self.api_version),
                None => format!("{s} using {}", self.api_version),
            };
        }
        s
    }
}

pub fn decode(obj: &Value) -> Vec<Entry> {
    obj.pointer("/metadata/managedFields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let set = match e.get("fieldsV1") {
                Some(f) => Set::from_fields_v1(f)?,
                None => Set::default(),
            };
            Some(Entry {
                manager: e.get("manager").and_then(Value::as_str).unwrap_or_default().to_string(),
                operation: e.get("operation").and_then(Value::as_str).unwrap_or("Update").to_string(),
                api_version: e.get("apiVersion").and_then(Value::as_str).unwrap_or_default().to_string(),
                time: e.get("time").and_then(Value::as_str).map(str::to_string),
                subresource: e.get("subresource").and_then(Value::as_str).unwrap_or_default().to_string(),
                set,
            })
        })
        .filter(|e| !e.set.is_empty())
        .collect()
}

/// At most this many Update entries; older ones merge into
/// `ancient-changes`.
const MAX_UPDATE_MANAGERS: usize = 10;

pub fn encode(mut entries: Vec<Entry>) -> Value {
    entries.retain(|e| !e.set.is_empty());
    let updates = entries.iter().filter(|e| e.operation == "Update").count();
    if updates > MAX_UPDATE_MANAGERS {
        let mut ups: Vec<Entry> = entries.iter().filter(|e| e.operation == "Update").cloned().collect();
        ups.sort_by(|a, b| a.time.cmp(&b.time));
        let excess = updates - MAX_UPDATE_MANAGERS + 1;
        let mut ancient = Entry {
            manager: "ancient-changes".into(),
            operation: "Update".into(),
            api_version: ups[0].api_version.clone(),
            time: None,
            subresource: String::new(),
            set: Set::default(),
        };
        for old in &ups[..excess] {
            ancient.set = ancient.set.union(&old.set);
            entries.retain(|e| !e.same_identity(old));
        }
        entries.push(ancient);
    }
    entries.sort_by(|a, b| {
        a.operation
            .cmp(&b.operation)
            .then_with(|| a.time.cmp(&b.time))
            .then_with(|| a.manager.cmp(&b.manager))
            .then_with(|| a.api_version.cmp(&b.api_version))
            .then_with(|| a.subresource.cmp(&b.subresource))
    });
    Value::Array(
        entries
            .into_iter()
            .map(|e| {
                let mut m = Map::new();
                m.insert("apiVersion".into(), json!(e.api_version));
                m.insert("fieldsType".into(), json!("FieldsV1"));
                m.insert("fieldsV1".into(), e.set.to_fields_v1());
                m.insert("manager".into(), json!(e.manager));
                m.insert("operation".into(), json!(e.operation));
                if !e.subresource.is_empty() {
                    m.insert("subresource".into(), json!(e.subresource));
                }
                if let Some(t) = e.time {
                    m.insert("time".into(), json!(t));
                }
                Value::Object(m)
            })
            .collect(),
    )
}

/// Paths no manager owns (the API server sets them).
fn strip(set: &mut Set) {
    for f in ["apiVersion", "kind", "metadata"] {
        set.members.remove(&Pe::Field(f.into()));
    }
    if let Some(meta) = set.children.get_mut(&Pe::Field("metadata".into())) {
        for f in ["name", "namespace", "creationTimestamp", "selfLink", "uid", "clusterName", "generation", "managedFields", "resourceVersion"] {
            meta.remove_subtree(&Pe::Field(f.into()));
        }
        if meta.is_empty() {
            set.children.remove(&Pe::Field("metadata".into()));
        }
    }
}

/// What a write to a resource (or its subresource) cannot change, so is
/// never owned through it.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub subresource: &'a str,
    /// The resource has a status subresource.
    pub has_status: bool,
}

fn reset(set: &mut Set, scope: Scope) {
    match scope.subresource {
        "status" => set.remove_subtree(&Pe::Field("spec".into())),
        "" if scope.has_status => set.remove_subtree(&Pe::Field("status".into())),
        _ => {}
    }
}

pub struct Request<'a> {
    pub manager: &'a str,
    pub api_version: &'a str,
    pub scope: Scope<'a>,
    pub now: &'a str,
}

/// managedFields after an update (create, update, patch) from `old` to
/// `new`: the manager owns what it added or changed, nobody else does.
pub fn update(old: &Value, new: &Value, base: Vec<Entry>, req: &Request, ty: Ty) -> Vec<Entry> {
    let cmp = compare(old, new, ty);
    let mut changed = cmp.added.union(&cmp.modified);
    strip(&mut changed);
    reset(&mut changed, req.scope);
    let mut entries = base;
    for e in entries.iter_mut() {
        e.set = e.set.difference(&changed).difference(&cmp.removed);
    }
    if !changed.is_empty() {
        let me = Entry {
            manager: req.manager.to_string(),
            operation: "Update".into(),
            api_version: req.api_version.to_string(),
            time: Some(req.now.to_string()),
            subresource: req.scope.subresource.to_string(),
            set: Set::default(),
        };
        match entries.iter_mut().find(|e| e.same_identity(&me)) {
            Some(e) => {
                e.set = e.set.union(&changed);
                e.time = me.time.clone();
            }
            None => entries.push(Entry { set: changed, ..me }),
        }
    }
    entries.retain(|e| !e.set.is_empty());
    entries
}

/// A server-side apply conflict: other managers own fields the apply
/// changes.
#[derive(Debug)]
pub struct Conflicts(pub Vec<(String, String)>);

impl Conflicts {
    /// The 409 message and causes, as the API server words them.
    pub fn message(&self) -> String {
        if let [(m, p)] = self.0.as_slice() {
            return format!("Apply failed with 1 conflict: conflict with {m}: {p}");
        }
        let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (m, p) in &self.0 {
            by.entry(m).or_default().push(p);
        }
        let mut lines = Vec::new();
        for (m, paths) in by {
            lines.push(format!("conflicts with {m}:"));
            for p in paths {
                lines.push(format!("- {p}"));
            }
        }
        format!("Apply failed with {} conflicts: {}", self.0.len(), lines.join("\n"))
    }

    pub fn causes(&self) -> Value {
        Value::Array(self.0.iter().map(|(m, p)| json!({"type": "FieldManagerConflict", "message": format!("conflict with {m}"), "field": p})).collect())
    }
}

pub enum ApplyError {
    Conflicts(Conflicts),
    Invalid(String),
}

/// Server-side apply of `config` onto `live` (`None` creates): the merged
/// object and the new managedFields.
pub fn apply(live: Option<&Value>, config: &Value, base: Vec<Entry>, req: &Request, force: bool, ty: Ty) -> Result<(Value, Vec<Entry>), ApplyError> {
    let empty = json!({});
    let live_v = live.unwrap_or(&empty);
    let mut entries = base;
    if entries.is_empty() {
        if let Some(l) = live {
            // An object nobody manages yet: what is there belongs to
            // "before-first-apply".
            let mut set = to_set(l, ty).map_err(ApplyError::Invalid)?;
            strip(&mut set);
            reset(&mut set, req.scope);
            if !set.is_empty() {
                entries.push(Entry {
                    manager: "before-first-apply".into(),
                    operation: "Update".into(),
                    api_version: req.api_version.to_string(),
                    time: Some(req.now.to_string()),
                    subresource: String::new(),
                    set,
                });
            }
        }
    }
    let merged = merge(Some(live_v), Some(config), ty).map_err(ApplyError::Invalid)?.unwrap_or_else(|| empty.clone());
    let mut new_set = to_set(config, ty).map_err(ApplyError::Invalid)?;
    strip(&mut new_set);
    reset(&mut new_set, req.scope);
    let me = Entry {
        manager: req.manager.to_string(),
        operation: "Apply".into(),
        api_version: req.api_version.to_string(),
        time: None,
        subresource: req.scope.subresource.to_string(),
        set: Set::default(),
    };
    let last = entries.iter().find(|e| e.same_identity(&me)).map(|e| e.set.clone());
    let previous_time = entries.iter().find(|e| e.same_identity(&me)).and_then(|e| e.time.clone());
    entries.retain(|e| !e.same_identity(&me));
    entries.push(Entry { set: new_set, ..me.clone() });
    let merged = match &last {
        Some(last) if !last.is_empty() => prune(&merged, &entries, last, ty)?,
        _ => merged,
    };
    // Conflicts: fields this apply changes that others own.
    let cmp = compare(live_v, &merged, ty);
    let changed = cmp.added.union(&cmp.modified);
    let mut conflicts = Vec::new();
    for e in entries.iter().filter(|e| !e.same_identity(&me)) {
        for p in e.set.intersection(&changed).paths() {
            conflicts.push((e.shown(), path_string(&p)));
        }
    }
    if !conflicts.is_empty() && !force {
        return Err(ApplyError::Conflicts(Conflicts(conflicts)));
    }
    for e in entries.iter_mut().filter(|e| !e.same_identity(&me)) {
        e.set = e.set.difference(&changed).difference(&cmp.removed);
    }
    let changed_object = live.is_none() || merged != *live_v;
    for e in entries.iter_mut().filter(|e| e.same_identity(&me)) {
        e.time = if changed_object || previous_time.is_none() { Some(req.now.to_string()) } else { previous_time.clone() };
    }
    entries.retain(|e| !e.set.is_empty());
    Ok((merged, entries))
}

/// Remove what the applier applied last time but not now, unless another
/// manager owns it.
fn prune(merged: &Value, entries: &[Entry], last: &Set, ty: Ty) -> Result<Value, ApplyError> {
    let last = ensure_named(last, Some(merged), ty);
    let pruned = remove_items(merged, &last, ty);
    // Add back what anyone (including this apply) owns.
    let managed = entries.iter().fold(Set::default(), |acc, e| acc.union(&e.set));
    let set_of = |v: &Value| to_set(v, ty).map(|s| ensure_named(&s, Some(v), ty)).map_err(ApplyError::Invalid);
    let merged_set = set_of(merged)?;
    let pruned_set = set_of(&pruned)?;
    let managed = ensure_named(&managed, Some(merged), ty);
    let pruned = remove_items(merged, &merged_set.difference(&pruned_set.union(&managed)), ty);
    // Add back what this applier never owned.
    let pruned_set = set_of(&pruned)?;
    Ok(remove_items(merged, &merged_set.difference(&pruned_set).intersection(&last), ty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(manager: &'a str, now: &'a str) -> Request<'a> {
        Request { manager, api_version: "v1", scope: Scope { subresource: "", has_status: false }, now }
    }

    #[test]
    fn fields_v1_roundtrip() {
        let v = json!({"f:metadata": {"f:labels": {".": {}, "f:app": {}}}, "f:spec": {"f:containers": {"k:{\"name\":\"nginx\"}": {".": {}, "f:image": {}, "f:name": {}}}}});
        let s = Set::from_fields_v1(&v).unwrap();
        assert_eq!(s.to_fields_v1(), v);
        assert!(s.has(&[Pe::Field("spec".into()), Pe::Field("containers".into()), Pe::Key("{\"name\":\"nginx\"}".into())]));
    }

    #[test]
    fn update_tracks_changes() {
        let ty = Ty::BuiltinRoot;
        let old = json!({});
        let new = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a", "labels": {"x": "1"}}, "data": {"k": "v"}});
        let e = update(&old, &new, vec![], &req("kubectl-create", "t1"), ty);
        assert_eq!(e[0].set.to_fields_v1(), json!({"f:data": {".": {}, "f:k": {}}, "f:metadata": {"f:labels": {".": {}, "f:x": {}}}}));
        let newer = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a", "labels": {"x": "1"}}, "data": {"k": "v2", "j": "w"}});
        let e = update(&new, &newer, e, &req("kubectl-edit", "t2"), ty);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].set.to_fields_v1(), json!({"f:data": {}, "f:metadata": {"f:labels": {".": {}, "f:x": {}}}}));
        assert_eq!(e[1].set.to_fields_v1(), json!({"f:data": {"f:j": {}, "f:k": {}}}));
    }

    #[test]
    fn apply_ownership() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "properties": {
            "a": {"type": "string"}, "b": {"type": "string"},
            "items": {"type": "array", "x-kubernetes-list-type": "map", "x-kubernetes-list-map-keys": ["name"],
                "items": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}, "v": {"type": "integer"}}}},
        }}}});
        let ty = Ty::CrdRoot(&schema);
        let cfg = |spec: Value| json!({"apiVersion": "x/v1", "kind": "X", "metadata": {"name": "o"}, "spec": spec});
        // alice creates.
        let (obj, e) = apply(None, &cfg(json!({"a": "1", "items": [{"name": "x", "v": 1}]})), vec![], &req("alice", "t1"), false, ty).ok().unwrap();
        // bob sets b and another item: no conflict.
        let (obj, e) = apply(Some(&obj), &cfg(json!({"b": "2", "items": [{"name": "y", "v": 2}]})), e, &req("bob", "t2"), false, ty).ok().unwrap();
        assert_eq!(obj["spec"], json!({"a": "1", "b": "2", "items": [{"name": "x", "v": 1}, {"name": "y", "v": 2}]}));
        // bob changing a conflicts with alice.
        match apply(Some(&obj), &cfg(json!({"a": "3", "b": "2", "items": [{"name": "y", "v": 2}]})), e.clone(), &req("bob", "t3"), false, ty) {
            Err(ApplyError::Conflicts(c)) => assert_eq!(c.message(), "Apply failed with 1 conflict: conflict with \"alice\": .spec.a"),
            _ => panic!("expected a conflict"),
        }
        // Forced, bob takes a over.
        let (obj, e) = apply(Some(&obj), &cfg(json!({"a": "3", "b": "2", "items": [{"name": "y", "v": 2}]})), e, &req("bob", "t3"), true, ty).ok().unwrap();
        assert_eq!(obj["spec"]["a"], "3");
        let alice = e.iter().find(|x| x.manager == "alice").unwrap();
        assert!(!alice.set.has(&[Pe::Field("spec".into()), Pe::Field("a".into())]));
        // alice stops applying her item: it goes; bob's stays.
        let (obj, e) = apply(Some(&obj), &cfg(json!({})), e, &req("alice", "t4"), false, ty).ok().unwrap();
        assert_eq!(obj["spec"], json!({"a": "3", "b": "2", "items": [{"name": "y", "v": 2}]}));
        assert!(e.iter().all(|x| x.manager != "alice"));
        // bob stops applying b: removed.
        let (obj, _) = apply(Some(&obj), &cfg(json!({"a": "3", "items": [{"name": "y", "v": 2}]})), e, &req("bob", "t5"), false, ty).ok().unwrap();
        assert_eq!(obj["spec"], json!({"a": "3", "items": [{"name": "y", "v": 2}]}));
    }

    #[test]
    fn list_merge_order() {
        let ty = Ty::Builtin("finalizers");
        let shape = Shape::Set;
        let out = merge_list(&[json!("a"), json!("b"), json!("c")], &[json!("c"), json!("d")], &shape, ty.items()).unwrap();
        assert_eq!(out, vec![json!("a"), json!("b"), json!("c"), json!("d")]);
    }
}
