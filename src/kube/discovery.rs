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
    subs
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
            let (kind, sub_group, sub_version) = if sub == "scale" {
                ("Scale", "autoscaling", "v1")
            } else {
                (rt.kind.as_str(), group, version)
            };
            resources.push(json!({
                "name": format!("{}/{sub}", rt.plural),
                "singularName": "",
                "namespaced": rt.namespaced,
                "group": sub_group,
                "version": sub_version,
                "kind": kind,
                "verbs": ["get", "patch", "update"],
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
                                    let kind = if sub == "scale" {
                                        json!({"group": "autoscaling", "version": "v1", "kind": "Scale"})
                                    } else {
                                        json!({"group": g, "version": v, "kind": rt.kind})
                                    };
                                    json!({"subresource": sub, "responseKind": kind, "verbs": ["get", "patch", "update"]})
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

/// `/openapi/v3`: one entry per group version.
pub fn openapi_root(registry: &Registry) -> Value {
    let mut paths = serde_json::Map::new();
    for (g, versions) in registry.groups() {
        for v in versions {
            let p = gv_path(&g, &v);
            paths.insert(
                p.clone(),
                json!({"serverRelativeURL": format!("/openapi/v3/{p}?hash={}", registry_hash(registry))}),
            );
        }
    }
    json!({"paths": paths})
}

/// Cache-buster for the per-group-version documents: changes when CRDs do.
fn registry_hash(registry: &Registry) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
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
pub fn openapi_group_version(registry: &Registry, group: &str, version: &str) -> Option<Value> {
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
    Some(json!({
        "openapi": "3.0.0",
        "info": {"title": "Kubernetes", "version": KUBE_GIT_VERSION},
        "paths": paths,
        "components": {"schemas": {}},
    }))
}
