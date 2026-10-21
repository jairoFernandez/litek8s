//! OpenAPI v2 (Swagger) JSON to the gnostic protobuf form client-go asks for
//! (`openapi_v2.Document`, github.com/google/gnostic-models OpenAPIv2.proto):
//! `discovery.OpenAPISchema()` only decodes that, and Argo CD's server-side
//! diff and kubectl's client-side checks build their types from it.
//!
//! Only what a schema registry needs is encoded: `swagger`, `info`, and the
//! definitions with every Schema field, vendor extensions included (as the
//! JSON text of the value in `Any.yaml`; JSON is YAML). Repeated fields of
//! two encoded Documents concatenate on decode, so a Document holding only
//! definitions can be appended to another one (the built-in definitions are
//! encoded at build time, the header and custom resources per request).
//!
//! Also compiled into build.rs (`#[path]`), so it only uses serde_json.

use serde_json::{Map, Value};

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    key(out, field, 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn string(out: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        bytes(out, field, s.as_bytes());
    }
}

fn message(out: &mut Vec<u8>, field: u32, f: impl FnOnce(&mut Vec<u8>)) {
    let mut m = Vec::new();
    f(&mut m);
    bytes(out, field, &m);
}

fn boolean(out: &mut Vec<u8>, field: u32, v: &Value) {
    if v.as_bool() == Some(true) {
        key(out, field, 0);
        varint(out, 1);
    }
}

fn int64(out: &mut Vec<u8>, field: u32, v: &Value) {
    // Zero is the proto3 default: not written, as decoders would drop it.
    if let Some(n) = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).filter(|n| *n != 0) {
        key(out, field, 0);
        varint(out, n as u64);
    }
}

fn double(out: &mut Vec<u8>, field: u32, v: &Value) {
    if let Some(f) = v.as_f64().filter(|f| *f != 0.0) {
        key(out, field, 1);
        out.extend_from_slice(&f.to_le_bytes());
    }
}

/// `Any { string yaml = 2; }`.
fn any(out: &mut Vec<u8>, field: u32, v: &Value) {
    message(out, field, |m| string(m, 2, &v.to_string()));
}

/// `NamedSchema { string name = 1; Schema value = 2; }` entries of a map.
fn named_schemas(out: &mut Vec<u8>, field: u32, map: &Map<String, Value>) {
    for (name, schema) in map {
        message(out, field, |m| {
            string(m, 1, name);
            message(m, 2, |s| schema_body(s, schema));
        });
    }
}

/// The fields of `Schema` (OpenAPIv2.proto field numbers).
fn schema_body(out: &mut Vec<u8>, schema: &Value) {
    let Some(obj) = schema.as_object() else { return };
    for (k, v) in obj {
        match k.as_str() {
            "$ref" => string(out, 1, v.as_str().unwrap_or_default()),
            "format" => string(out, 2, v.as_str().unwrap_or_default()),
            "title" => string(out, 3, v.as_str().unwrap_or_default()),
            "description" => string(out, 4, v.as_str().unwrap_or_default()),
            "default" => any(out, 5, v),
            "multipleOf" => double(out, 6, v),
            "maximum" => double(out, 7, v),
            "exclusiveMaximum" => boolean(out, 8, v),
            "minimum" => double(out, 9, v),
            "exclusiveMinimum" => boolean(out, 10, v),
            "maxLength" => int64(out, 11, v),
            "minLength" => int64(out, 12, v),
            "pattern" => string(out, 13, v.as_str().unwrap_or_default()),
            "maxItems" => int64(out, 14, v),
            "minItems" => int64(out, 15, v),
            "uniqueItems" => boolean(out, 16, v),
            "maxProperties" => int64(out, 17, v),
            "minProperties" => int64(out, 18, v),
            "required" => {
                for r in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
                    bytes(out, 19, r.as_bytes());
                }
            }
            "enum" => {
                for e in v.as_array().into_iter().flatten() {
                    any(out, 20, e);
                }
            }
            // AdditionalPropertiesItem { oneof { Schema schema = 1; bool boolean = 2; } }
            "additionalProperties" => message(out, 21, |m| match v {
                Value::Bool(b) => {
                    key(m, 2, 0);
                    varint(m, u64::from(*b));
                }
                _ => message(m, 1, |s| schema_body(s, v)),
            }),
            // TypeItem { repeated string value = 1; }
            "type" => message(out, 22, |m| match v {
                Value::Array(types) => types.iter().filter_map(Value::as_str).for_each(|t| bytes(m, 1, t.as_bytes())),
                _ => string(m, 1, v.as_str().unwrap_or_default()),
            }),
            // ItemsItem { repeated Schema schema = 1; }
            "items" => message(out, 23, |m| match v {
                Value::Array(items) => items.iter().for_each(|i| message(m, 1, |s| schema_body(s, i))),
                _ => message(m, 1, |s| schema_body(s, v)),
            }),
            "allOf" => {
                for s in v.as_array().into_iter().flatten() {
                    message(out, 24, |m| schema_body(m, s));
                }
            }
            // Properties { repeated NamedSchema additional_properties = 1; }
            "properties" => {
                if let Some(props) = v.as_object() {
                    message(out, 25, |m| named_schemas(m, 1, props));
                }
            }
            "discriminator" => string(out, 26, v.as_str().unwrap_or_default()),
            "readOnly" => boolean(out, 27, v),
            // ExternalDocs { string description = 1; string url = 2; }
            "externalDocs" => message(out, 29, |m| {
                string(m, 1, v.get("description").and_then(Value::as_str).unwrap_or_default());
                string(m, 2, v.get("url").and_then(Value::as_str).unwrap_or_default());
            }),
            "example" => any(out, 30, v),
            // NamedAny { string name = 1; Any value = 2; }
            x if x.starts_with("x-") => message(out, 31, |m| {
                string(m, 1, x);
                any(m, 2, v);
            }),
            // OpenAPI v3 keywords (nullable, oneOf, ...) have no v2 field.
            _ => {}
        }
    }
}

/// A Document with only `definitions` (field 9).
pub fn definitions(defs: &Map<String, Value>) -> Vec<u8> {
    let mut out = Vec::new();
    // Definitions { repeated NamedSchema additional_properties = 1; }
    message(&mut out, 9, |m| named_schemas(m, 1, defs));
    out
}

/// The start of a Document: `swagger`, `info` and empty `paths`, for
/// definitions appended after it.
pub fn header(title: &str, version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    string(&mut out, 1, "2.0");
    // Info { string title = 1; string version = 2; }
    message(&mut out, 2, |m| {
        string(m, 1, title);
        string(m, 2, version);
    });
    message(&mut out, 8, |_| {});
    out
}
