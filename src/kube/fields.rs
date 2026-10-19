//! Server-side field validation (`?fieldValidation=Strict|Warn|Ignore`) and
//! custom resource schema checks.
//!
//! - duplicate keys in JSON and YAML bodies,
//! - fields a built-in type does not have (checked against the protobuf
//!   schemas, see [`super::protobuf::unknown_fields`]),
//! - fields a CRD's structural schema does not declare,
//! - CRD schema value rules: types, `enum`, `required`, `nullable`, string
//!   and number bounds.

use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Dotted paths of keys that appear twice in the same JSON object.
pub fn json_duplicates(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut de = serde_json::Deserializer::from_slice(body);
    let _ = Walk { path: String::new(), out: &mut out }.deserialize(&mut de);
    out
}

struct Walk<'a> {
    path: String,
    out: &'a mut Vec<String>,
}

impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut i = 0;
        while seq.next_element_seed(Walk { path: format!("{}[{i}]", self.path), out: &mut *self.out })?.is_some() {
            i += 1;
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let path = if self.path.is_empty() { key.clone() } else { format!("{}.{key}", self.path) };
            if !seen.insert(key) && !self.out.contains(&path) {
                self.out.push(path.clone());
            }
            map.next_value_seed(Walk { path, out: &mut *self.out })?;
        }
        Ok(())
    }
}

/// `line N: key "k" already set in map` for duplicate keys in a YAML body,
/// as the API server's YAML decoder reports them.
pub fn yaml_duplicates(body: &[u8]) -> Vec<String> {
    match serde_yaml::from_slice::<serde_yaml::Value>(body) {
        Err(e) => {
            let msg = e.to_string();
            let Some((_, rest)) = msg.split_once("duplicate entry with key ") else { return Vec::new() };
            let key = rest.split(" at line ").next().unwrap_or_default();
            // The error points at the map; the line reported is the second
            // occurrence of the key among that map's entries.
            let (start, column) = e.location().map(|l| (l.line(), l.column())).unwrap_or((1, 1));
            let text = String::from_utf8_lossy(body);
            let bare = key.trim_matches('"');
            let indent = column.saturating_sub(1);
            let mut seen = 0;
            let mut line = start;
            for (i, l) in text.lines().enumerate().skip(start.saturating_sub(1)) {
                let lead = l.len() - l.trim_start().len();
                if !l.trim().is_empty() && lead < indent {
                    break;
                }
                let entry = l.trim_start().trim_start_matches("- ");
                if lead == indent && (entry.starts_with(&format!("{bare}:")) || entry.starts_with(&format!("{key}:"))) {
                    seen += 1;
                    if seen == 2 {
                        line = i + 1;
                        break;
                    }
                }
            }
            vec![format!("line {line}: key {key} already set in map")]
        }
        Ok(_) => Vec::new(),
    }
}

const OBJECT_META: &[&str] = &[
    "name",
    "generateName",
    "namespace",
    "selfLink",
    "uid",
    "resourceVersion",
    "generation",
    "creationTimestamp",
    "deletionTimestamp",
    "deletionGracePeriodSeconds",
    "labels",
    "annotations",
    "ownerReferences",
    "finalizers",
    "managedFields",
];

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn bool_at(schema: &Value, key: &str) -> bool {
    schema.get(key).and_then(Value::as_bool) == Some(true)
}

/// Paths (`spec.foo`) of fields the CRD schema does not declare. Root and
/// embedded-resource `apiVersion`/`kind`/`metadata` are always allowed; their
/// metadata may only carry ObjectMeta fields.
pub fn unknown_by_schema(obj: &Value, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_schema(obj, schema, "", true, &mut out);
    out
}

fn walk_schema(v: &Value, schema: &Value, path: &str, resource: bool, out: &mut Vec<String>) {
    if bool_at(schema, "x-kubernetes-int-or-string") {
        return;
    }
    match v {
        Value::Object(map) => {
            let preserve = bool_at(schema, "x-kubernetes-preserve-unknown-fields");
            let props = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (k, child) in map {
                let p = join(path, k);
                if resource && (k == "apiVersion" || k == "kind") {
                    continue;
                }
                if resource && k == "metadata" {
                    if let Some(m) = child.as_object() {
                        for mk in m.keys() {
                            if !OBJECT_META.contains(&mk.as_str()) {
                                out.push(join(&p, mk));
                            }
                        }
                    }
                    continue;
                }
                match props.and_then(|ps| ps.get(k)) {
                    Some(s) => walk_schema(child, s, &p, bool_at(s, "x-kubernetes-embedded-resource"), out),
                    None => match additional {
                        Some(a) if a.is_object() => walk_schema(child, a, &p, false, out),
                        Some(Value::Bool(true)) => {}
                        _ if preserve => {}
                        _ => out.push(p),
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    walk_schema(item, s, &format!("{path}[{i}]"), bool_at(s, "x-kubernetes-embedded-resource"), out);
                }
            }
        }
        _ => {}
    }
}

/// Remove the fields the schema does not declare (CRD pruning).
pub fn prune(obj: &mut Value, schema: &Value) {
    prune_at(obj, schema, true);
}

fn prune_at(v: &mut Value, schema: &Value, resource: bool) {
    if bool_at(schema, "x-kubernetes-int-or-string") {
        return;
    }
    match v {
        Value::Object(map) => {
            let preserve = bool_at(schema, "x-kubernetes-preserve-unknown-fields");
            let props = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            map.retain(|k, child| {
                if resource && (k == "apiVersion" || k == "kind" || k == "metadata") {
                    return true;
                }
                match props.and_then(|ps| ps.get(k)) {
                    Some(s) => {
                        prune_at(child, s, bool_at(s, "x-kubernetes-embedded-resource"));
                        true
                    }
                    None => match additional {
                        Some(a) if a.is_object() => {
                            prune_at(child, a, false);
                            true
                        }
                        Some(Value::Bool(true)) => true,
                        _ => preserve,
                    },
                }
            });
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for item in items {
                    prune_at(item, s, bool_at(s, "x-kubernetes-embedded-resource"));
                }
            }
        }
        _ => {}
    }
}

/// Fill in `default`s the schema declares for missing fields (also inside
/// array items and additionalProperties), like the API server does on
/// requests and on reads from storage. As there, a `null` in a field that is
/// not `nullable` counts as missing: it gets the default, or is dropped (Argo
/// CD clears `operation` with a JSON patch that adds `null`).
pub fn apply_defaults(v: &mut Value, schema: &Value) {
    let null_not_allowed = |child: &Value, s: &Value| child.is_null() && !bool_at(s, "nullable");
    match v {
        Value::Object(map) => {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (k, s) in props {
                    if map.get(k).is_some_and(|child| null_not_allowed(child, s)) {
                        map.remove(k);
                    }
                    if !map.contains_key(k) {
                        if let Some(d) = s.get("default") {
                            map.insert(k.clone(), d.clone());
                        }
                    }
                    if let Some(child) = map.get_mut(k) {
                        apply_defaults(child, s);
                    }
                }
            }
            if let Some(add) = schema.get("additionalProperties").filter(|a| a.is_object()) {
                let known: Vec<String> = schema.get("properties").and_then(Value::as_object).map(|p| p.keys().cloned().collect()).unwrap_or_default();
                map.retain(|k, child| known.contains(k) || !null_not_allowed(child, add) || add.get("default").is_some());
                for (k, child) in map.iter_mut() {
                    if !known.contains(k) {
                        if null_not_allowed(child, add) {
                            *child = add["default"].clone();
                        }
                        apply_defaults(child, add);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for item in items {
                    if null_not_allowed(item, s) {
                        if let Some(d) = s.get("default") {
                            *item = d.clone();
                        }
                    }
                    apply_defaults(item, s);
                }
            }
        }
        _ => {}
    }
}

/// Value errors against an OpenAPI v3 (structural) schema, in the API
/// server's `path: Reason: detail` form.
pub fn schema_errors(obj: &Value, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(obj, schema, "", true, &mut out);
    out
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check(v: &Value, schema: &Value, path: &str, root: bool, out: &mut Vec<String>) {
    let p = if path.is_empty() { "<root>".to_string() } else { path.to_string() };
    if bool_at(schema, "x-kubernetes-int-or-string") {
        if !matches!(v, Value::String(_) | Value::Number(_) | Value::Null) {
            out.push(format!("{p}: Invalid value: \"{}\": {p} in body must be of type int-or-string: \"{}\"", type_of(v), type_of(v)));
        }
        return;
    }
    if v.is_null() {
        if !bool_at(schema, "nullable") && schema.get("type").is_some() && !root {
            out.push(format!("{p}: Invalid value: \"null\": {p} in body must be of type {}: \"null\"", schema["type"].as_str().unwrap_or_default()));
        }
        return;
    }
    if let Some(t) = schema.get("type").and_then(Value::as_str) {
        let got = type_of(v);
        let ok = got == t || (t == "number" && got == "integer");
        if !ok {
            out.push(format!("{p}: Invalid value: \"{got}\": {p} in body must be of type {t}: \"{got}\""));
            return;
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(v) {
            let shown = |x: &Value| match x {
                Value::String(s) => format!("\"{s}\""),
                other => other.to_string(),
            };
            let supported: Vec<String> = values.iter().map(shown).collect();
            out.push(format!("{p}: Unsupported value: {}: supported values: {}", shown(v), supported.join(", ")));
        }
    }
    match v {
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64).filter(|m| len > *m) {
                out.push(format!("{p}: Too long: may not be more than {max} bytes"));
            }
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64).filter(|m| len < *m) {
                out.push(format!("{p}: Invalid value: \"{s}\": {p} in body should be at least {min} chars long"));
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                let excl = bool_at(schema, "exclusiveMaximum");
                if x > max || (excl && x == max) {
                    out.push(format!("{p}: Invalid value: {n}: {p} in body should be less than or equal to {max}"));
                }
            }
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                let excl = bool_at(schema, "exclusiveMinimum");
                if x < min || (excl && x == min) {
                    out.push(format!("{p}: Invalid value: {n}: {p} in body should be greater than or equal to {min}"));
                }
            }
        }
        Value::Object(map) => {
            for r in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    out.push(format!("{}: Required value", join(path, r)));
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for (k, child) in map {
                if root && (k == "apiVersion" || k == "kind" || k == "metadata") {
                    continue;
                }
                let child_schema = props.and_then(|ps| ps.get(k)).or_else(|| schema.get("additionalProperties").filter(|a| a.is_object()));
                if let Some(s) = child_schema {
                    check(child, s, &join(path, k), bool_at(s, "x-kubernetes-embedded-resource"), out);
                }
            }
            if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64).filter(|m| map.len() as u64 > *m) {
                out.push(format!("{p}: Too many: {}: must have at most {max} items", map.len()));
            }
        }
        Value::Array(items) => {
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64).filter(|m| items.len() as u64 > *m) {
                out.push(format!("{p}: Too many: {}: must have at most {max} items", items.len()));
            }
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64).filter(|m| (items.len() as u64) < *m) {
                out.push(format!("{p}: Invalid value: {}: {p} in body should have at least {min} items", items.len()));
            }
            if let Some(s) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(item, s, &format!("{path}[{i}]"), bool_at(s, "x-kubernetes-embedded-resource"), out);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duplicates() {
        let body = br#"{"spec": {"unknownField": "foo", "replicas": 2, "replicas": 3, "list": [{"a": 1, "a": 2}]}}"#;
        assert_eq!(json_duplicates(body), vec!["spec.replicas", "spec.list[0].a"]);
        let yaml = b"\napiVersion: v1\nkind: X\nmetadata:\n  name: a\nspec:\n  unknown: uk1\n  foo: foo1\n  foo: foo2\n";
        assert_eq!(yaml_duplicates(yaml), vec!["line 9: key \"foo\" already set in map"]);
    }

    #[test]
    fn schema_checks() {
        let schema = json!({"type": "object", "properties": {
            "spec": {"type": "object", "properties": {
                "bars": {"type": "array", "items": {"type": "object", "required": ["name"], "properties": {
                    "name": {"type": "string"},
                    "feeling": {"type": "string", "enum": ["Great", "Down"]},
                }}},
                "template": {"type": "object", "x-kubernetes-embedded-resource": true, "properties": {"spec": {"type": "object"}}},
            }},
        }});
        let obj = json!({"apiVersion": "x/v1", "kind": "X", "metadata": {"name": "a", "unknownMeta": 1}, "unknownField": 1,
            "spec": {"foo": 1, "bars": [{"feeling": "NonExistentValue"}], "template": {"metadata": {"bad": 1}, "spec": {}}}});
        assert_eq!(
            unknown_by_schema(&obj, &schema),
            vec!["metadata.unknownMeta", "spec.bars[0]", "spec.foo", "spec.template.metadata.bad", "unknownField"]
                .into_iter()
                .filter(|p| *p != "spec.bars[0]")
                .collect::<Vec<_>>()
        );
        let errs = schema_errors(&obj, &schema);
        assert!(errs.contains(&"spec.bars[0].name: Required value".to_string()), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains(r#"Unsupported value: "NonExistentValue""#)), "{errs:?}");
        let mut pruned = obj.clone();
        prune(&mut pruned, &schema);
        assert!(pruned.get("unknownField").is_none() && pruned["spec"].get("foo").is_none());
        assert_eq!(pruned["metadata"]["unknownMeta"], 1);
    }

    #[test]
    fn nulls_in_non_nullable_fields_are_defaulted_or_dropped() {
        let schema = json!({"type": "object", "properties": {
            "operation": {"type": "object", "properties": {"x": {"type": "string"}}},
            "spec": {"type": "object", "properties": {
                "mode": {"type": "string", "default": "auto"},
                "note": {"type": "string", "nullable": true},
                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
            }},
        }});
        let mut obj = json!({"operation": null, "spec": {"mode": null, "note": null, "labels": {"a": null, "b": "x"}}});
        apply_defaults(&mut obj, &schema);
        assert_eq!(obj, json!({"spec": {"mode": "auto", "note": null, "labels": {"b": "x"}}}));
        assert!(schema_errors(&obj, &schema).is_empty());
    }
}
