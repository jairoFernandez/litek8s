//! Discovery documents: `/api`, `/apis`, per-group-version resource lists,
//! aggregated discovery (one request instead of one per group version). The
//! OpenAPI documents are in `openapi.rs`.

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

pub(crate) fn gv_path(group: &str, version: &str) -> String {
    if group.is_empty() {
        format!("api/{version}")
    } else {
        format!("apis/{group}/{version}")
    }
}
