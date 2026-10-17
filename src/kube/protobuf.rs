//! Decode Kubernetes protobuf bodies (`application/vnd.kubernetes.protobuf`)
//! into the same JSON the server stores. kubectl's typed clients (`kubectl
//! create`, `run`, `expose`, ...) send built-in objects this way and offer no
//! opt-out, so the server must read it. Responses stay JSON: clients always
//! list `application/json` as an accepted fallback.
//!
//! Wire format: the magic `k8s\0`, then a `runtime.Unknown` message whose
//! `raw` field holds the object encoded with the type's generated.proto
//! schema. Field names in those schemas are the JSON names, so the conversion
//! is generic except for a handful of apimachinery types with custom JSON.

use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use base64::Engine;
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, MessageDescriptor, ReflectMessage, Value as Pv};
use serde_json::{Map, Value, json};

const MAGIC: &[u8] = b"k8s\0";
const META: &str = "k8s.io.apimachinery.pkg.apis.meta.v1";
const APIEXT: &str = "k8s.io.apiextensions_apiserver.pkg.apis.apiextensions.v1";

fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(&include_bytes!(concat!(env!("OUT_DIR"), "/k8s-descriptors.bin"))[..])
            .expect("embedded Kubernetes descriptor set is valid")
    })
}

pub fn is_protobuf(body: &[u8]) -> bool {
    body.starts_with(MAGIC)
}

/// Decode a protobuf-encoded Kubernetes object into JSON (with apiVersion and
/// kind filled in from the envelope).
pub fn decode(body: &[u8]) -> Result<Value> {
    let Some(payload) = body.strip_prefix(MAGIC) else {
        bail!("not a Kubernetes protobuf body (missing magic prefix)");
    };
    let unknown_desc = pool()
        .get_message_by_name("k8s.io.apimachinery.pkg.runtime.Unknown")
        .context("runtime.Unknown missing from descriptor set")?;
    let unknown = DynamicMessage::decode(unknown_desc, payload).context("invalid protobuf envelope")?;
    let type_meta = unknown
        .get_field_by_name("typeMeta")
        .and_then(|v| v.as_message().cloned());
    let field = |m: &DynamicMessage, name: &str| {
        m.get_field_by_name(name)
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    let (api_version, kind) = match &type_meta {
        Some(tm) => (field(tm, "apiVersion"), field(tm, "kind")),
        None => (String::new(), String::new()),
    };
    let raw = unknown
        .get_field_by_name("raw")
        .and_then(|v| v.as_bytes().cloned())
        .unwrap_or_default();
    let desc = message_for(&api_version, &kind)
        .with_context(|| format!("no protobuf schema for {api_version} {kind}"))?;
    let msg = DynamicMessage::decode(desc, raw.as_ref())
        .with_context(|| format!("invalid protobuf {kind}"))?;
    let mut value = to_json(&msg);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("apiVersion".into(), json!(api_version));
        obj.insert("kind".into(), json!(kind));
    }
    Ok(value)
}

/// The generated.proto message for a GroupVersionKind, e.g. `apps/v1
/// Deployment` -> `k8s.io.api.apps.v1.Deployment`.
fn message_for(api_version: &str, kind: &str) -> Option<MessageDescriptor> {
    let (group, version) = api_version.rsplit_once('/').unwrap_or(("", api_version));
    let short = if group.is_empty() {
        "core"
    } else {
        group.split('.').next().unwrap_or(group)
    };
    [
        format!("k8s.io.api.{short}.{version}.{kind}"),
        // CustomResourceDefinition lives outside k8s.io/api.
        format!("k8s.io.{short}_apiserver.pkg.apis.{short}.{version}.{kind}"),
        format!("{META}.{kind}"),
    ]
    .iter()
    .find_map(|name| pool().get_message_by_name(name))
}

fn to_json(msg: &DynamicMessage) -> Value {
    let get = |name: &str| msg.get_field_by_name(name);
    match msg.descriptor().full_name() {
        name if name == format!("{META}.Time") || name == format!("{META}.MicroTime") => {
            let secs = get("seconds").and_then(|v| v.as_i64()).unwrap_or(0);
            let nanos = get("nanos").and_then(|v| v.as_i32()).unwrap_or(0);
            if secs == 0 && nanos == 0 {
                return Value::Null;
            }
            let Some(t) = chrono::DateTime::from_timestamp(secs, nanos.max(0) as u32) else {
                return Value::Null;
            };
            if name.ends_with("MicroTime") {
                json!(t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
            } else {
                json!(t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            }
        }
        "k8s.io.apimachinery.pkg.util.intstr.IntOrString" => {
            if get("type").and_then(|v| v.as_i64()).unwrap_or(0) == 1 {
                json!(get("strVal").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
            } else {
                json!(get("intVal").and_then(|v| v.as_i32()).unwrap_or(0))
            }
        }
        "k8s.io.apimachinery.pkg.api.resource.Quantity" => {
            json!(get("string").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
        }
        name if name == format!("{META}.Duration") => {
            json!(get("duration").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
        }
        name if name == "k8s.io.apimachinery.pkg.runtime.RawExtension" || name == format!("{META}.FieldsV1") => {
            let raw = get("raw").or_else(|| get("Raw")).and_then(|v| v.as_bytes().cloned()).unwrap_or_default();
            serde_json::from_slice(&raw).unwrap_or(Value::Null)
        }
        // apiextensions' JSON-schema types marshal to JSON by hand in Go.
        name if name == format!("{APIEXT}.JSON") => {
            let raw = get("raw").and_then(|v| v.as_bytes().cloned()).unwrap_or_default();
            serde_json::from_slice(&raw).unwrap_or(Value::Null)
        }
        name if name == format!("{APIEXT}.JSONSchemaPropsOrArray") => match get("jSONSchemas").map(|v| scalar_or_nested(&v)) {
            Some(Value::Array(items)) if !items.is_empty() => Value::Array(items),
            _ => get("schema").map(|v| scalar_or_nested(&v)).unwrap_or(Value::Null),
        },
        name if name == format!("{APIEXT}.JSONSchemaPropsOrBool") => match msg.has_field_by_name("schema") {
            true => get("schema").map(|v| scalar_or_nested(&v)).unwrap_or(Value::Null),
            false => json!(get("allows").and_then(|v| v.as_bool()).unwrap_or(false)),
        },
        name if name == format!("{APIEXT}.JSONSchemaPropsOrStringArray") => match msg.has_field_by_name("schema") {
            true => get("schema").map(|v| scalar_or_nested(&v)).unwrap_or(Value::Null),
            false => get("property").map(|v| scalar_or_nested(&v)).unwrap_or(json!([])),
        },
        name if name == format!("{APIEXT}.JSONSchemaProps") => {
            let mut out = Map::new();
            for (field, value) in msg.fields() {
                let key = match field.name() {
                    "schema" => "$schema".to_string(),
                    "ref" => "$ref".to_string(),
                    other => match other.strip_prefix("xKubernetes") {
                        // xKubernetesListMapKeys -> x-kubernetes-list-map-keys
                        Some(rest) => rest.chars().fold("x-kubernetes".to_string(), |mut acc, c| {
                            if c.is_ascii_uppercase() {
                                acc.push('-');
                            }
                            acc.push(c.to_ascii_lowercase());
                            acc
                        }),
                        None => other.to_string(),
                    },
                };
                out.insert(key, scalar_or_nested(value));
            }
            Value::Object(out)
        }
        _ => {
            let mut out = Map::new();
            for (field, value) in msg.fields() {
                let v = scalar_or_nested(value);
                // gogo-protobuf always writes non-pointer strings, even empty
                // ones; JSON omits them, and absent == "" for every field.
                if v == json!("") || v.is_null() {
                    continue;
                }
                out.insert(field.name().to_string(), v);
            }
            Value::Object(out)
        }
    }
}

fn scalar_or_nested(value: &Pv) -> Value {
    match value {
        Pv::Bool(b) => json!(b),
        Pv::I32(n) => json!(n),
        Pv::I64(n) => json!(n),
        Pv::U32(n) => json!(n),
        Pv::U64(n) => json!(n),
        Pv::F32(n) => json!(n),
        Pv::F64(n) => json!(n),
        Pv::String(s) => json!(s),
        Pv::Bytes(b) => json!(base64::engine::general_purpose::STANDARD.encode(b)),
        Pv::EnumNumber(n) => json!(n),
        Pv::Message(m) => to_json(m),
        Pv::List(items) => Value::Array(items.iter().map(scalar_or_nested).collect()),
        Pv::Map(entries) => {
            let mut out = Map::new();
            for (k, v) in entries {
                let key = match k {
                    MapKey::String(s) => s.clone(),
                    MapKey::Bool(b) => b.to_string(),
                    MapKey::I32(n) => n.to_string(),
                    MapKey::I64(n) => n.to_string(),
                    MapKey::U32(n) => n.to_string(),
                    MapKey::U64(n) => n.to_string(),
                };
                out.insert(key, scalar_or_nested(v));
            }
            Value::Object(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// Encode a JSON-ish object the way client-go would, by filling a
    /// DynamicMessage, then wrapping it in the runtime.Unknown envelope.
    fn envelope(api_version: &str, kind: &str, raw: Vec<u8>) -> Vec<u8> {
        let mut unknown = DynamicMessage::new(pool().get_message_by_name("k8s.io.apimachinery.pkg.runtime.Unknown").unwrap());
        let mut tm = DynamicMessage::new(pool().get_message_by_name("k8s.io.apimachinery.pkg.runtime.TypeMeta").unwrap());
        tm.set_field_by_name("apiVersion", Pv::String(api_version.into()));
        tm.set_field_by_name("kind", Pv::String(kind.into()));
        unknown.set_field_by_name("typeMeta", Pv::Message(tm));
        unknown.set_field_by_name("raw", Pv::Bytes(raw.into()));
        let mut out = MAGIC.to_vec();
        out.extend(unknown.encode_to_vec());
        out
    }

    #[test]
    fn decodes_a_configmap() {
        let desc = pool().get_message_by_name("k8s.io.api.core.v1.ConfigMap").unwrap();
        let mut cm = DynamicMessage::new(desc);
        let mut meta = DynamicMessage::new(pool().get_message_by_name(&format!("{META}.ObjectMeta")).unwrap());
        meta.set_field_by_name("name", Pv::String("app".into()));
        meta.set_field_by_name("namespace", Pv::String(String::new()));
        cm.set_field_by_name("metadata", Pv::Message(meta));
        let mut data = std::collections::HashMap::new();
        data.insert(MapKey::String("color".into()), Pv::String("blue".into()));
        cm.set_field_by_name("data", Pv::Map(data));
        let body = envelope("v1", "ConfigMap", cm.encode_to_vec());
        let v = decode(&body).unwrap();
        assert_eq!(
            v,
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "app"}, "data": {"color": "blue"}})
        );
    }

    #[test]
    fn decodes_intorstring_and_quantity() {
        let pool = pool();
        let mut port = DynamicMessage::new(pool.get_message_by_name("k8s.io.api.core.v1.ServicePort").unwrap());
        let mut ios = DynamicMessage::new(pool.get_message_by_name("k8s.io.apimachinery.pkg.util.intstr.IntOrString").unwrap());
        ios.set_field_by_name("type", Pv::I64(1));
        ios.set_field_by_name("strVal", Pv::String("http".into()));
        port.set_field_by_name("port", Pv::I32(80));
        port.set_field_by_name("targetPort", Pv::Message(ios));
        assert_eq!(to_json(&port), json!({"port": 80, "targetPort": "http"}));
        assert!(message_for("apps/v1", "Deployment").is_some());
        assert!(message_for("networking.k8s.io/v1", "Ingress").is_some());
        assert!(message_for("v1", "DeleteOptions").is_some());
        // Operators check their permissions with these (client-go sends protobuf).
        assert!(message_for("authorization.k8s.io/v1", "SelfSubjectAccessReview").is_some());
        assert!(message_for("authentication.k8s.io/v1", "TokenReview").is_some());
        assert!(message_for("apiextensions.k8s.io/v1", "CustomResourceDefinition").is_some());
    }

    #[test]
    fn decodes_json_schema_props() {
        let pool = pool();
        let msg = |name: &str| DynamicMessage::new(pool.get_message_by_name(&format!("{APIEXT}.{name}")).unwrap());
        let mut string = msg("JSONSchemaProps");
        string.set_field_by_name("type", Pv::String("string".into()));
        let mut default = msg("JSON");
        default.set_field_by_name("raw", Pv::Bytes(br#""a""#.to_vec().into()));
        string.set_field_by_name("default", Pv::Message(default));
        let mut items = msg("JSONSchemaPropsOrArray");
        items.set_field_by_name("schema", Pv::Message(string.clone()));
        let mut list = msg("JSONSchemaProps");
        list.set_field_by_name("type", Pv::String("array".into()));
        list.set_field_by_name("items", Pv::Message(items));
        list.set_field_by_name("xKubernetesListType", Pv::String("set".into()));
        let mut free = msg("JSONSchemaPropsOrBool");
        free.set_field_by_name("allows", Pv::Bool(true));
        let mut root = msg("JSONSchemaProps");
        root.set_field_by_name("type", Pv::String("object".into()));
        root.set_field_by_name("xKubernetesPreserveUnknownFields", Pv::Bool(true));
        root.set_field_by_name("additionalProperties", Pv::Message(free));
        let mut props = std::collections::HashMap::new();
        props.insert(MapKey::String("tags".into()), Pv::Message(list));
        root.set_field_by_name("properties", Pv::Map(props));
        assert_eq!(
            to_json(&root),
            json!({
                "type": "object",
                "x-kubernetes-preserve-unknown-fields": true,
                "additionalProperties": true,
                "properties": {"tags": {"type": "array", "x-kubernetes-list-type": "set", "items": {"type": "string", "default": "a"}}}
            })
        );
    }
}
