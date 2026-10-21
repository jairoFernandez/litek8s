//! OpenAPI documents: `/openapi/v2` (JSON and the gnostic protobuf form) and
//! `/openapi/v3` with one document per group version.
//!
//! The built-in types' definitions are upstream's (Kubernetes 1.33,
//! `openapi/swagger.json`, definitions only), embedded gzipped by build.rs as
//! compact JSON and as protobuf; custom resources' schemas are added per
//! request. Nothing is kept in memory between requests: each one inflates
//! what it needs (about 1 MB) and splices instead of building a JSON tree,
//! one at a time (`RENDER`), so a burst of clients costs one request's
//! memory, not one per client the allocator then keeps.
//! Schema-driven clients need the definitions: client-go's `OpenAPISchema()`
//! (Argo CD's structured-merge-diff types for server-side apply and diff,
//! kubectl's patch metadata), `kubectl explain` (v3).
//!
//! The v3 documents keep the `fieldValidation` parameter on every write
//! path, so kubectl leaves field validation to the server.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::discovery::{KUBE_GIT_VERSION, gv_path};
use super::openapi_pb;
use super::registry::{Registry, ResourceType};

static DEFINITIONS_JSON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/openapi-v2-definitions.json.gz"));
static DEFINITIONS_PB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/openapi-v2-definitions.pb.gz"));
/// Changes with the embedded definitions (part of the v3 cache-busting hash).
const DEFINITIONS_HASH: &str = env!("LITEK8S_OPENAPI_HASH");

const META_DEF: &str = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";

/// Held while a document is built from the built-in definitions.
static RENDER: Mutex<()> = Mutex::new(());

fn render<T>(f: impl FnOnce() -> T) -> T {
    let _one_at_a_time = RENDER.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

pub const PROTOBUF_TYPE: &str = "application/com.github.proto-openapi.spec.v2.v1.0+protobuf";

fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data).read_to_end(&mut out).expect("embedded OpenAPI definitions");
    out
}

/// The built-in definitions as one compact JSON object.
fn builtin_text() -> String {
    String::from_utf8(gunzip(DEFINITIONS_JSON)).expect("embedded OpenAPI definitions are UTF-8")
}

/// The built-in definitions by name, borrowed from `builtin_text()`.
fn builtin_map(text: &str) -> BTreeMap<&str, &RawValue> {
    serde_json::from_str(text).expect("embedded OpenAPI definitions are a JSON object")
}

/// `group/version/Kind` as an OpenAPI definition name, the way the API
/// server writes it: `stable.example.com/v1/Foo` -> `com.example.stable.v1.Foo`.
pub fn definition_name(group: &str, version: &str, kind: &str) -> String {
    let mut parts: Vec<&str> = group.split('.').filter(|p| !p.is_empty()).collect();
    parts.reverse();
    parts.push(version);
    parts.push(kind);
    parts.join(".")
}

/// The served versions of the CRDs that carry a schema, as
/// (group, version, kind, openAPIV3Schema).
fn crd_schemas(crds: &[Value]) -> Vec<(String, String, String, Value)> {
    let mut out = Vec::new();
    for crd in crds {
        let s = |p: &str| crd.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        let (group, kind) = (s("/spec/group"), s("/spec/names/kind"));
        for v in crd.pointer("/spec/versions").and_then(Value::as_array).into_iter().flatten() {
            if v.get("served").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(schema) = v.pointer("/schema/openAPIV3Schema").filter(|s| s.is_object()) else { continue };
            let version = v.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            out.push((group.clone(), version, kind.clone(), schema.clone()));
        }
    }
    out
}

/// A CRD schema as published: the user's schema plus the standard
/// apiVersion/kind/metadata fields and the group-version-kind extension.
fn published_schema(schema: &Value, group: &str, version: &str, kind: &str, meta_ref: &str, v3: bool) -> Value {
    let mut out = schema.clone();
    if out.get("type").and_then(Value::as_str).is_none_or(|t| t == "object") {
        let props = out.as_object_mut().unwrap().entry("properties").or_insert_with(|| json!({}));
        if let Some(props) = props.as_object_mut() {
            props.insert("apiVersion".into(), json!({"type": "string", "description": "APIVersion defines the versioned schema of this representation of an object. Servers should convert recognized schemas to the latest internal value, and may reject unrecognized values. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#resources"}));
            props.insert("kind".into(), json!({"type": "string", "description": "Kind is a string value representing the REST resource this object represents. Servers may infer this from the endpoint the client submits requests to. Cannot be updated. In CamelCase. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#types-kinds"}));
            let meta = if v3 {
                json!({"allOf": [{"$ref": meta_ref}], "description": "Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata"})
            } else {
                json!({"$ref": meta_ref, "description": "Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata"})
            };
            props.insert("metadata".into(), meta);
        }
    }
    out["x-kubernetes-group-version-kind"] = json!([{"group": group, "version": version, "kind": kind}]);
    out
}

fn strip_preserve(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("x-kubernetes-preserve-unknown-fields");
            for child in m.values_mut() {
                strip_preserve(child);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_preserve),
        _ => {}
    }
}

/// The custom resources' OpenAPI v2 definitions.
fn crd_definitions_v2(crds: &[Value]) -> Map<String, Value> {
    let mut defs = Map::new();
    for (g, v, k, mut schema) in crd_schemas(crds) {
        // OpenAPI v2 has no x-kubernetes-preserve-unknown-fields: it is
        // dropped there (v3 keeps it).
        strip_preserve(&mut schema);
        defs.insert(definition_name(&g, &v, &k), published_schema(&schema, &g, &v, &k, &format!("#/definitions/{META_DEF}"), false));
    }
    defs
}

fn info() -> Value {
    json!({"title": "Kubernetes", "version": KUBE_GIT_VERSION})
}

/// `/openapi/v2` as JSON: the built-in definitions and the custom resources'.
pub fn openapi_v2(crds: &[Value]) -> Vec<u8> {
    let crds: Vec<(String, String)> = crd_definitions_v2(crds).into_iter().map(|(name, schema)| (Value::String(name).to_string(), schema.to_string())).collect();
    render(|| {
        let builtin = builtin_text();
        let len = builtin.len() + crds.iter().map(|(n, s)| n.len() + s.len() + 2).sum::<usize>() + 128;
        let mut out = String::with_capacity(len);
        out.push_str(&format!(r#"{{"swagger":"2.0","info":{},"paths":{{}},"definitions":"#, info()));
        // The built-in object without its closing brace, then the CRDs' entries.
        out.push_str(&builtin[..builtin.len() - 1]);
        for (name, schema) in &crds {
            out.push(',');
            out.push_str(name);
            out.push(':');
            out.push_str(schema);
        }
        out.push_str("}}");
        out.into_bytes()
    })
}

/// `/openapi/v2` in the gnostic protobuf form client-go's `OpenAPISchema()`
/// decodes: header, built-in definitions, then the custom resources' (the
/// repeated definitions of the concatenated messages merge on decode).
pub fn openapi_v2_protobuf(crds: &[Value]) -> Vec<u8> {
    let crds = openapi_pb::definitions(&crd_definitions_v2(crds));
    render(|| {
        let mut out = openapi_pb::header("Kubernetes", KUBE_GIT_VERSION);
        flate2::read::GzDecoder::new(DEFINITIONS_PB).read_to_end(&mut out).expect("embedded OpenAPI definitions");
        out.extend(crds);
        out
    })
}

/// `/openapi/v3`: one entry per group version.
pub fn openapi_root(registry: &Registry, crds: &[Value]) -> Value {
    let hash = registry_hash(registry, crds);
    let mut paths = Map::new();
    for (g, versions) in registry.groups() {
        for v in versions {
            let p = gv_path(&g, &v);
            paths.insert(p.clone(), json!({"serverRelativeURL": format!("/openapi/v3/{p}?hash={hash}")}));
        }
    }
    json!({"paths": paths})
}

/// Cache-buster for the per-group-version documents: changes when CRDs or
/// the built-in definitions do.
pub fn registry_hash(registry: &Registry, crds: &[Value]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    DEFINITIONS_HASH.hash(&mut h);
    for crd in crds {
        crd.pointer("/metadata/resourceVersion").map(|v| v.to_string()).hash(&mut h);
    }
    for (g, vs) in registry.groups() {
        g.hash(&mut h);
        for v in vs {
            v.hash(&mut h);
            for rt in registry.resources(&g, &v) {
                rt.plural.hash(&mut h);
            }
        }
    }
    format!("{:016X}", h.finish())
}

/// The definitions a `$ref`s in compact JSON.
fn refs(raw: &str) -> impl Iterator<Item = &str> {
    const REF: &str = r##""$ref":"#/definitions/"##;
    raw.match_indices(REF).filter_map(|(i, _)| raw[i + REF.len()..].split('"').next())
}

/// The built-in definitions of one group version's kinds and everything
/// they reference (plus ObjectMeta's closure when `meta`), as OpenAPI v3
/// component schemas.
fn builtin_schemas_v3(group: &str, version: &str, meta: bool) -> BTreeMap<String, Box<RawValue>> {
    #[derive(Deserialize)]
    struct Gvks {
        #[serde(rename = "x-kubernetes-group-version-kind", default)]
        gvks: Vec<Gvk>,
    }
    #[derive(Deserialize)]
    struct Gvk {
        #[serde(default)]
        group: String,
        version: String,
    }
    let text = builtin_text();
    let defs = builtin_map(&text);
    let mut todo: Vec<&str> = defs
        .iter()
        .filter(|(_, raw)| {
            serde_json::from_str::<Gvks>(raw.get()).is_ok_and(|g| g.gvks.iter().any(|x| x.group == group && x.version == version))
        })
        .map(|(name, _)| *name)
        .collect();
    if meta {
        todo.push(META_DEF);
    }
    let mut seen = BTreeSet::new();
    while let Some(name) = todo.pop() {
        if let Some(raw) = defs.get(name)
            && seen.insert(name)
        {
            todo.extend(refs(raw.get()));
        }
    }
    seen.into_iter()
        .map(|name| {
            let raw = defs[name].get().replace("\"#/definitions/", "\"#/components/schemas/");
            (name.to_string(), RawValue::from_string(raw).expect("valid JSON"))
        })
        .collect()
}

#[derive(Serialize)]
struct V3Document {
    openapi: &'static str,
    info: Value,
    paths: Map<String, Value>,
    components: V3Components,
}

#[derive(Serialize)]
struct V3Components {
    schemas: BTreeMap<String, Box<RawValue>>,
}

/// A per-group-version OpenAPI v3 document: write paths with the
/// `fieldValidation` parameter and the schemas of the group version's kinds
/// (built-in or custom resources) with what they reference.
pub fn openapi_group_version(registry: &Registry, crds: &[Value], group: &str, version: &str) -> Option<Vec<u8>> {
    if !registry.has_group_version(group, version) {
        return None;
    }
    let params = json!([
        {"name": "dryRun", "in": "query", "schema": {"type": "string"}},
        {"name": "fieldManager", "in": "query", "schema": {"type": "string"}},
        {"name": "fieldValidation", "in": "query", "schema": {"type": "string"}},
    ]);
    let op = |gvk_action: &str, rt: &ResourceType| {
        json!({
            "parameters": params,
            "x-kubernetes-action": gvk_action,
            "x-kubernetes-group-version-kind": {"group": group, "version": version, "kind": rt.kind},
            "responses": {"200": {"description": "OK"}},
        })
    };
    let mut paths = Map::new();
    let base = format!("/{}", gv_path(group, version));
    for rt in registry.resources(group, version) {
        let (collection, item) = if rt.namespaced {
            (
                format!("{base}/namespaces/{{namespace}}/{}", rt.plural),
                format!("{base}/namespaces/{{namespace}}/{}/{{name}}", rt.plural),
            )
        } else {
            (format!("{base}/{}", rt.plural), format!("{base}/{}/{{name}}", rt.plural))
        };
        paths.insert(collection, json!({"post": op("post", &rt)}));
        paths.insert(item, json!({"put": op("put", &rt), "patch": op("patch", &rt)}));
    }
    let crd_schemas: Vec<_> = crd_schemas(crds).into_iter().filter(|(g, v, ..)| g == group && v == version).collect();
    render(|| {
        let mut schemas = builtin_schemas_v3(group, version, !crd_schemas.is_empty());
        for (g, v, k, schema) in crd_schemas {
            let schema = published_schema(&schema, &g, &v, &k, &format!("#/components/schemas/{META_DEF}"), true);
            schemas.insert(definition_name(&g, &v, &k), serde_json::value::to_raw_value(&schema).expect("JSON"));
        }
        let doc = V3Document { openapi: "3.0.0", info: info(), paths, components: V3Components { schemas } };
        Some(serde_json::to_vec(&doc).expect("JSON"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use prost_reflect::{DescriptorPool, DynamicMessage, Value as Pv};

    fn crd() -> Value {
        json!({
            "metadata": {"name": "widgets.example.com", "resourceVersion": "7"},
            "spec": {
                "group": "example.com",
                "names": {"kind": "Widget", "plural": "widgets"},
                "scope": "Namespaced",
                "versions": [{
                    "name": "v1", "served": true, "storage": true,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                        "spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": true,
                                 "properties": {"size": {"type": "integer", "minimum": 1}}},
                    }}},
                }],
            },
        })
    }

    fn gvk(schema: &Value) -> Vec<(String, String, String)> {
        let s = |g: &Value, k: &str| g.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        schema["x-kubernetes-group-version-kind"].as_array().into_iter().flatten().map(|g| (s(g, "group"), s(g, "version"), s(g, "kind"))).collect()
    }

    #[test]
    fn v2_json_has_builtin_and_crd_definitions() {
        let doc: Value = serde_json::from_slice(&openapi_v2(&[crd()])).unwrap();
        assert_eq!(doc["swagger"], "2.0");
        let defs = doc["definitions"].as_object().unwrap();
        assert!(defs.len() > 600, "{} definitions", defs.len());
        let sa = &defs["io.k8s.api.core.v1.ServiceAccount"];
        assert_eq!(gvk(sa), vec![("".into(), "v1".into(), "ServiceAccount".into())]);
        assert_eq!(gvk(&defs["io.k8s.api.rbac.v1.Role"]), vec![("rbac.authorization.k8s.io".into(), "v1".into(), "Role".into())]);
        assert_eq!(gvk(&defs["io.k8s.api.apps.v1.Deployment"])[0].2, "Deployment");
        let widget = &defs["com.example.v1.Widget"];
        assert_eq!(gvk(widget), vec![("example.com".into(), "v1".into(), "Widget".into())]);
        assert_eq!(widget["properties"]["metadata"]["$ref"], format!("#/definitions/{META_DEF}"));
        assert!(widget["properties"]["spec"].get("x-kubernetes-preserve-unknown-fields").is_none());
        // Every reference resolves.
        let text = doc["definitions"].to_string();
        for r in refs(&text) {
            assert!(defs.contains_key(r), "unresolved {r}");
        }
    }

    fn gnostic_pool() -> DescriptorPool {
        let files = protox::compile(["OpenAPIv2.proto"], ["openapi"]).unwrap();
        DescriptorPool::from_file_descriptor_set(files).unwrap()
    }

    fn field<'a>(m: &'a DynamicMessage, name: &str) -> std::borrow::Cow<'a, Pv> {
        m.get_field_by_name(name).unwrap_or_else(|| panic!("no field {name}"))
    }

    /// `repeated NamedSchema additional_properties = 1` (Definitions,
    /// Properties) by name.
    fn named(m: &DynamicMessage) -> BTreeMap<String, DynamicMessage> {
        field(m, "additional_properties")
            .as_list()
            .unwrap()
            .iter()
            .map(|named| {
                let named = named.as_message().unwrap();
                (field(named, "name").as_str().unwrap().to_string(), field(named, "value").as_message().unwrap().clone())
            })
            .collect()
    }

    fn pb_extension(schema: &DynamicMessage, name: &str) -> Option<Value> {
        let list = field(schema, "vendor_extension");
        list.as_list().unwrap().iter().find_map(|e| {
            let e = e.as_message().unwrap();
            (field(e, "name").as_str() == Some(name)).then(|| {
                let any = field(e, "value");
                serde_json::from_str(field(any.as_message().unwrap(), "yaml").as_str().unwrap()).unwrap()
            })
        })
    }

    #[test]
    fn v2_protobuf_decodes_as_a_gnostic_document() {
        let pool = gnostic_pool();
        let desc = pool.get_message_by_name("openapi.v2.Document").unwrap();
        let doc = DynamicMessage::decode(desc, &openapi_v2_protobuf(&[crd()])[..]).unwrap();
        assert_eq!(field(&doc, "swagger").as_str(), Some("2.0"));
        let info = field(&doc, "info");
        assert_eq!(field(info.as_message().unwrap(), "version").as_str(), Some(KUBE_GIT_VERSION));
        let defs = named(field(&doc, "definitions").as_message().unwrap());
        let json: Value = serde_json::from_slice(&openapi_v2(&[crd()])).unwrap();
        assert_eq!(defs.len(), json["definitions"].as_object().unwrap().len());

        let sa = &defs["io.k8s.api.core.v1.ServiceAccount"];
        assert_eq!(pb_extension(sa, "x-kubernetes-group-version-kind"), Some(json!([{"group": "", "kind": "ServiceAccount", "version": "v1"}])));
        let props = named(field(sa, "properties").as_message().unwrap());
        let secrets = &props["secrets"];
        assert_eq!(pb_extension(secrets, "x-kubernetes-patch-strategy"), Some(json!("merge")));
        assert_eq!(pb_extension(secrets, "x-kubernetes-patch-merge-key"), Some(json!("name")));
        let ty = field(secrets, "type");
        assert_eq!(field(ty.as_message().unwrap(), "value").as_list().unwrap()[0].as_str(), Some("array"));
        let items = field(secrets, "items");
        let items = field(items.as_message().unwrap(), "schema");
        let item = items.as_list().unwrap()[0].as_message().unwrap().clone();
        assert_eq!(field(&item, "_ref").as_str(), Some("#/definitions/io.k8s.api.core.v1.ObjectReference"));

        let widget = &defs["com.example.v1.Widget"];
        assert_eq!(pb_extension(widget, "x-kubernetes-group-version-kind"), Some(json!([{"group": "example.com", "kind": "Widget", "version": "v1"}])));
        let props = named(field(widget, "properties").as_message().unwrap());
        let spec_props = named(field(&props["spec"], "properties").as_message().unwrap());
        assert_eq!(field(&spec_props["size"], "minimum").as_f64(), Some(1.0));
        // Nothing is lost or unknown: re-encoding the built-in definitions
        // gives as many bytes (in field-number order).
        let builtin = gunzip(DEFINITIONS_PB);
        let desc = pool.get_message_by_name("openapi.v2.Document").unwrap();
        assert_eq!(DynamicMessage::decode(desc, &builtin[..]).unwrap().encoded_len(), builtin.len());
    }

    fn registry_with_crd() -> Registry {
        let registry = Registry::with_builtins();
        registry.register_crd(&crd()).unwrap();
        registry
    }

    #[test]
    fn v3_documents_have_the_group_versions_schemas() {
        let registry = registry_with_crd();
        let doc: Value = serde_json::from_slice(&openapi_group_version(&registry, &[crd()], "apps", "v1").unwrap()).unwrap();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let deploy = &schemas["io.k8s.api.apps.v1.Deployment"];
        assert_eq!(gvk(deploy), vec![("apps".into(), "v1".into(), "Deployment".into())]);
        assert!(schemas.contains_key("io.k8s.api.core.v1.PodSpec"));
        assert!(!schemas.contains_key("io.k8s.api.batch.v1.Job"));
        assert!(!schemas.contains_key("com.example.v1.Widget"));
        let text = doc["components"].to_string();
        assert!(!text.contains("#/definitions/"));
        const REF: &str = r##""$ref":"#/components/schemas/"##;
        for (i, _) in text.match_indices(REF) {
            let name = text[i + REF.len()..].split('"').next().unwrap();
            assert!(schemas.contains_key(name), "unresolved {name}");
        }
        // The write paths still advertise server-side field validation.
        let patch = &doc["paths"]["/apis/apps/v1/namespaces/{namespace}/deployments/{name}"]["patch"];
        assert!(patch["parameters"].as_array().unwrap().iter().any(|p| p["name"] == "fieldValidation"));

        let doc: Value = serde_json::from_slice(&openapi_group_version(&registry, &[crd()], "example.com", "v1").unwrap()).unwrap();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        assert_eq!(gvk(&schemas["com.example.v1.Widget"])[0].2, "Widget");
        assert!(schemas.contains_key(META_DEF));
        assert!(schemas.contains_key("io.k8s.apimachinery.pkg.apis.meta.v1.ManagedFieldsEntry"));
        assert!(!schemas.contains_key("io.k8s.api.core.v1.Pod"));
        assert!(openapi_group_version(&registry, &[crd()], "example.com", "v2").is_none());
    }

    #[test]
    fn v3_hash_changes_with_crds() {
        let registry = registry_with_crd();
        let a = registry_hash(&registry, &[crd()]);
        let mut changed = crd();
        changed["metadata"]["resourceVersion"] = json!("8");
        assert_ne!(a, registry_hash(&registry, &[changed]));
        assert_eq!(a, registry_hash(&registry, &[crd()]));
        let root = openapi_root(&registry, &[crd()]);
        assert_eq!(root["paths"]["apis/apps/v1"]["serverRelativeURL"], format!("/openapi/v3/apis/apps/v1?hash={a}"));
    }
}
