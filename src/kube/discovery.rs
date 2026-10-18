//! Discovery documents: `/api`, `/apis`, per-group-version resource lists,
//! aggregated discovery (one request instead of one per group version), and a
//! minimal `/openapi/v3` that tells kubectl the server validates fields itself.

use serde_json::{Value, json};

use super::registry::{Registry, ResourceType};

pub const KUBE_MAJOR: &str = "1";
pub const KUBE_MINOR: &str = "33";
pub const KUBE_GIT_VERSION: &str = "v1.33.0+litek8s";

pub fn version() -> Value {
    json!({
        "major": KUBE_MAJOR,
        "minor": KUBE_MINOR,
        "gitVersion": KUBE_GIT_VERSION,
        "gitCommit": env!("CARGO_PKG_VERSION"),
        "gitTreeState": "clean",
        "buildDate": "1970-01-01T00:00:00Z",
        "goVersion": "rust",
        "compiler": "rustc",
        "platform": format!("{}/{}", std::env::consts::OS, go_arch()),
    })
}

fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

pub fn api_versions(server_address: &str) -> Value {
    json!({
        "kind": "APIVersions",
        "versions": ["v1"],
        "serverAddressByClientCIDRs": [{"clientCIDR": "0.0.0.0/0", "serverAddress": server_address}],
    })
}

fn group_entry(group: &str, versions: &[String]) -> Value {
    let versions: Vec<Value> = versions
        .iter()
        .map(|v| json!({"groupVersion": format!("{group}/{v}"), "version": v}))
        .collect();
    json!({
        "name": group,
        "versions": versions,
        "preferredVersion": versions[0],
    })
}

pub fn group_list(registry: &Registry) -> Value {
    let groups: Vec<Value> = registry
        .groups()
        .into_iter()
        .filter(|(g, _)| !g.is_empty())
        .map(|(g, vs)| group_entry(&g, &vs))
        .collect();
    json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": groups})
}

pub fn group(registry: &Registry, name: &str) -> Option<Value> {
    let (g, vs) = registry.groups().into_iter().find(|(g, _)| g == name)?;
    let mut entry = group_entry(&g, &vs);
    entry["kind"] = json!("APIGroup");
    entry["apiVersion"] = json!("v1");
    Some(entry)
}

fn subresources(rt: &ResourceType) -> Vec<&'static str> {
    let mut subs = Vec::new();
    if rt.has_status {
        subs.push("status");
    }
    if rt.has_scale {
        subs.push("scale");
    }
    match (rt.group.as_str(), rt.plural.as_str()) {
        ("", "pods") => subs.extend(["attach", "binding", "ephemeralcontainers", "eviction", "exec", "log", "portforward", "proxy"]),
        ("", "services") => subs.push("proxy"),
        ("", "namespaces") => subs.push("finalize"),
        ("certificates.k8s.io", "certificatesigningrequests") => subs.push("approval"),
        _ => {}
    }
    subs
}

/// (group, version, kind) a subresource answers with.
fn sub_kind(rt: &ResourceType, sub: &str) -> (String, String, String) {
    let own = (rt.group.clone(), rt.version.clone(), rt.kind.clone());
    let other = |g: &str, v: &str, k: &str| (g.to_string(), v.to_string(), k.to_string());
    match sub {
        "scale" => other("autoscaling", "v1", "Scale"),
        "eviction" => other("policy", "v1", "Eviction"),
        "binding" => other("", "v1", "Binding"),
        "attach" => other("", "v1", "PodAttachOptions"),
        "exec" => other("", "v1", "PodExecOptions"),
        "portforward" => other("", "v1", "PodPortForwardOptions"),
        "proxy" if rt.plural == "pods" => other("", "v1", "PodProxyOptions"),
        "proxy" => other("", "v1", "ServiceProxyOptions"),
        _ => own,
    }
}

fn sub_verbs(sub: &str) -> Vec<&'static str> {
    match sub {
        "eviction" | "binding" => vec!["create"],
        "attach" | "exec" | "portforward" => vec!["create", "get"],
        "proxy" => vec!["create", "delete", "get", "patch", "update"],
        "log" => vec!["get"],
        "finalize" => vec!["update"],
        _ => vec!["get", "patch", "update"],
    }
}

pub fn resource_list(registry: &Registry, group: &str, version: &str) -> Value {
    let mut resources = Vec::new();
    for rt in registry.resources(group, version) {
        resources.push(json!({
            "name": rt.plural,
            "singularName": rt.singular,
            "namespaced": rt.namespaced,
            "kind": rt.kind,
            "verbs": rt.verbs(),
            "shortNames": rt.short_names,
            "categories": rt.categories,
        }));
        for sub in subresources(&rt) {
            let (sub_group, sub_version, kind) = sub_kind(&rt, sub);
            resources.push(json!({
                "name": format!("{}/{sub}", rt.plural),
                "singularName": "",
                "namespaced": rt.namespaced,
                "group": sub_group,
                "version": sub_version,
                "kind": kind,
                "verbs": sub_verbs(sub),
            }));
        }
    }
    let group_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": group_version,
        "resources": resources,
    })
}

/// `apidiscovery.k8s.io/v2` APIGroupDiscoveryList for either the core group
/// (`/api`) or every named group (`/apis`).
pub fn aggregated(registry: &Registry, core: bool) -> Value {
    let items: Vec<Value> = registry
        .groups()
        .into_iter()
        .filter(|(g, _)| g.is_empty() == core)
        .map(|(g, versions)| {
            let versions: Vec<Value> = versions
                .iter()
                .map(|v| {
                    let resources: Vec<Value> = registry
                        .resources(&g, v)
                        .iter()
                        .map(|rt| {
                            let subs: Vec<Value> = subresources(rt)
                                .into_iter()
                                .map(|sub| {
                                    let (sg, sv, sk) = sub_kind(rt, sub);
                                    let kind = json!({"group": sg, "version": sv, "kind": sk});
                                    json!({"subresource": sub, "responseKind": kind, "verbs": sub_verbs(sub)})
                                })
                                .collect();
                            let mut r = json!({
                                "resource": rt.plural,
                                "responseKind": {"group": g, "version": v, "kind": rt.kind},
                                "scope": if rt.namespaced { "Namespaced" } else { "Cluster" },
                                "singularResource": rt.singular,
                                "verbs": rt.verbs(),
                            });
                            if !rt.short_names.is_empty() {
                                r["shortNames"] = json!(rt.short_names);
                            }
                            if !rt.categories.is_empty() {
                                r["categories"] = json!(rt.categories);
                            }
                            if !subs.is_empty() {
                                r["subresources"] = json!(subs);
                            }
                            r
                        })
                        .collect();
                    json!({"version": v, "resources": resources, "freshness": "Current"})
                })
                .collect();
            json!({"metadata": {"name": g}, "versions": versions})
        })
        .collect();
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "metadata": {},
        "items": items,
    })
}

fn gv_path(group: &str, version: &str) -> String {
    if group.is_empty() {
        format!("api/{version}")
    } else {
        format!("apis/{group}/{version}")
    }
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

const META_DEF: &str = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";

/// The served versions of the CRDs that carry a schema, as
/// (group, version, kind, openAPIV3Schema).
fn crd_schemas(crds: &[Value]) -> Vec<(String, String, String, Value)> {
    let mut out = Vec::new();
    for crd in crds {
        let s = |p: &str| crd.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        let (group, kind) = (s("/spec/group"), s("/spec/names/kind"));
        for v in crd.pointer("/spec/versions").and_then(Value::as_array).into_iter().flatten() {
            if v.get("served").and_then(Value::as_bool) == Some(false) {
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

/// The parts of ObjectMeta's schema `kubectl explain` shows.
fn object_meta_schema() -> Value {
    let f = |t: &str, d: &str| json!({"type": t, "description": d});
    json!({
        "description": "ObjectMeta is metadata that all persisted resources must have, which includes all objects users must create.",
        "type": "object",
        "properties": {
            "annotations": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Annotations is an unstructured key value map stored with a resource that may be set by external tools to store and retrieve arbitrary metadata."},
            "creationTimestamp": f("string", "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC."),
            "deletionGracePeriodSeconds": f("integer", "Number of seconds allowed for this object to gracefully terminate before it will be removed from the system."),
            "deletionTimestamp": f("string", "DeletionTimestamp is RFC 3339 date and time at which this resource will be deleted."),
            "finalizers": {"type": "array", "items": {"type": "string"}, "description": "Must be empty before the object is deleted from the registry."},
            "generateName": f("string", "GenerateName is an optional prefix, used by the server, to generate a unique name ONLY IF the Name field has not been provided."),
            "generation": f("integer", "A sequence number representing a specific generation of the desired state."),
            "labels": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Map of string keys and values that can be used to organize and categorize (scope and select) objects."},
            "name": f("string", "Name must be unique within a namespace."),
            "namespace": f("string", "Namespace defines the space within which each name must be unique."),
            "ownerReferences": {"type": "array", "items": {"type": "object"}, "description": "List of objects depended by this object."},
            "resourceVersion": f("string", "An opaque value that represents the internal version of this object."),
            "uid": f("string", "UID is the unique in time and space value for this object."),
        },
    })
}

/// `/openapi/v2` as JSON: the definitions of custom resources.
pub fn openapi_v2(crds: &[Value]) -> Value {
    let mut defs = serde_json::Map::new();
    defs.insert(META_DEF.into(), object_meta_schema());
    for (g, v, k, schema) in crd_schemas(crds) {
        defs.insert(definition_name(&g, &v, &k), published_schema(&schema, &g, &v, &k, &format!("#/definitions/{META_DEF}"), false));
    }
    json!({"swagger": "2.0", "info": {"title": "Kubernetes", "version": KUBE_GIT_VERSION}, "paths": {}, "definitions": defs})
}

/// `/openapi/v3`: one entry per group version.
pub fn openapi_root(registry: &Registry, crds: &[Value]) -> Value {
    let mut paths = serde_json::Map::new();
    for (g, versions) in registry.groups() {
        for v in versions {
            let p = gv_path(&g, &v);
            paths.insert(
                p.clone(),
                json!({"serverRelativeURL": format!("/openapi/v3/{p}?hash={}", registry_hash(registry, crds))}),
            );
        }
    }
    json!({"paths": paths})
}

/// Cache-buster for the per-group-version documents: changes when CRDs do.
fn registry_hash(registry: &Registry, crds: &[Value]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
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

/// A per-group-version OpenAPI v3 document. It has paths but no schemas: its
/// job is to advertise the `fieldValidation` query parameter, so kubectl
/// leaves validation to the server instead of downloading schemas.
pub fn openapi_group_version(registry: &Registry, crds: &[Value], group: &str, version: &str) -> Option<Value> {
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
    let mut paths = serde_json::Map::new();
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
        paths.insert(
            item,
            json!({"put": op("put", &rt), "patch": op("patch", &rt)}),
        );
    }
    let mut schemas = serde_json::Map::new();
    for (g, v, k, schema) in crd_schemas(crds).into_iter().filter(|(g, v, ..)| g == group && v == version) {
        schemas.insert(definition_name(&g, &v, &k), published_schema(&schema, &g, &v, &k, &format!("#/components/schemas/{META_DEF}"), true));
    }
    if !schemas.is_empty() {
        schemas.insert(META_DEF.into(), object_meta_schema());
    }
    Some(json!({
        "openapi": "3.0.0",
        "info": {"title": "Kubernetes", "version": KUBE_GIT_VERSION},
        "paths": paths,
        "components": {"schemas": schemas},
    }))
}
