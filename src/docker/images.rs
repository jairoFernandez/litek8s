//! Docker's view of the node's image store: image IDs are config digests,
//! names are shown short (`nginx:latest`), untagged images in use by a
//! container stay as `<none>` under their ID.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::kube::agent::crun::image::{ImageStore, IndexEntry, Reference};

/// Keys of the build cache (intermediate images); never listed.
pub const BUILD_CACHE_PREFIX: &str = "build-cache/";

/// One image (several keys may point at it).
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// `sha256:<config digest>`.
    pub id: String,
    pub keys: Vec<String>,
    pub entry: IndexEntry,
}

impl ImageInfo {
    pub fn tags(&self) -> Vec<String> {
        let mut t: Vec<String> = self.keys.iter().filter_map(|k| short_name(k)).filter(|n| !n.contains('@')).collect();
        t.sort();
        t.dedup();
        t
    }

    pub fn digests(&self) -> Vec<String> {
        let mut d: Vec<String> = self
            .keys
            .iter()
            .filter(|_| !self.entry.loaded)
            .filter_map(|k| short_name(k))
            .map(|n| {
                let repo = match n.split_once('@') {
                    Some((r, _)) => r.to_string(),
                    None => strip_tag(&n),
                };
                format!("{repo}@{}", self.entry.top)
            })
            .collect();
        d.sort();
        d.dedup();
        d
    }
}

fn strip_tag(name: &str) -> String {
    match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo.to_string(),
        _ => name.to_string(),
    }
}

/// `docker.io/library/nginx:latest` -> `nginx:latest`; untagged and cache
/// keys -> None.
pub fn short_name(key: &str) -> Option<String> {
    if key.starts_with("sha256:") || key.starts_with(BUILD_CACHE_PREFIX) {
        return None;
    }
    let rest = key.strip_prefix("docker.io/").map(|r| r.strip_prefix("library/").unwrap_or(r)).unwrap_or(key);
    Some(rest.to_string())
}

/// The store key for a name (`nginx` -> `docker.io/library/nginx:latest`).
pub fn key_for(name: &str) -> Option<String> {
    Reference::parse(name).ok().map(|r| r.canonical())
}

pub fn all(store: &ImageStore) -> Vec<ImageInfo> {
    let mut by_id: BTreeMap<String, ImageInfo> = BTreeMap::new();
    for (key, entry) in store.entries() {
        if key.starts_with(BUILD_CACHE_PREFIX) {
            continue;
        }
        let info = by_id.entry(entry.config.clone()).or_insert_with(|| ImageInfo { id: entry.config.clone(), keys: Vec::new(), entry: entry.clone() });
        info.keys.push(key);
    }
    by_id.into_values().collect()
}

pub fn by_id(store: &ImageStore, id: &str) -> Option<ImageInfo> {
    all(store).into_iter().find(|i| i.id == id)
}

/// Find an image the way Docker does: by name (with a tag or digest), then
/// by (a prefix of) its ID.
pub fn resolve(store: &ImageStore, reference: &str) -> Option<ImageInfo> {
    let images = all(store);
    if let Ok(r) = Reference::parse(reference) {
        let key = r.canonical();
        if let Some(i) = images.iter().find(|i| i.keys.contains(&key)) {
            return Some(i.clone());
        }
        if let Some(d) = &r.digest {
            let repo = format!("{}/{}", r.registry, r.repository);
            if let Some(i) = images.iter().find(|i| {
                (i.entry.top == *d || i.entry.manifest == *d) && i.keys.iter().any(|k| k.starts_with(&format!("{repo}:")) || k.starts_with(&format!("{repo}@")))
            }) {
                return Some(i.clone());
            }
        }
    }
    let hex = reference.strip_prefix("sha256:").unwrap_or(reference);
    if !hex.is_empty() && hex.len() <= 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let matches: Vec<&ImageInfo> = images.iter().filter(|i| i.id.trim_start_matches("sha256:").starts_with(hex)).collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
    }
    None
}

fn config_time(config: &Value) -> i64 {
    config["created"].as_str().and_then(super::model::parse_time).map(|t| t.timestamp()).unwrap_or(0)
}

fn size_of(store: &ImageStore, entry: &IndexEntry) -> u64 {
    entry.layers.iter().map(|l| store.layer_size(l)).sum()
}

/// One entry of `GET /images/json`.
pub fn summary(store: &ImageStore, info: &ImageInfo, containers: i64) -> Value {
    let config = store.config_blob(&info.entry.config).unwrap_or(Value::Null);
    let size = size_of(store, &info.entry);
    json!({
        "Id": info.id,
        "ParentId": "",
        "RepoTags": info.tags(),
        "RepoDigests": info.digests(),
        "Created": config_time(&config),
        "Size": size,
        "SharedSize": -1,
        "VirtualSize": size,
        "Labels": config.pointer("/config/Labels").cloned().unwrap_or(Value::Null),
        "Containers": containers,
    })
}

/// `GET /images/{name}/json`.
pub fn inspect(store: &ImageStore, info: &ImageInfo) -> Value {
    let config = store.config_blob(&info.entry.config).unwrap_or(Value::Null);
    let size = size_of(store, &info.entry);
    let image_config = config.get("config").cloned().unwrap_or(json!({}));
    json!({
        "Id": info.id,
        "RepoTags": info.tags(),
        "RepoDigests": info.digests(),
        "Parent": "",
        "Comment": config["comment"].as_str().unwrap_or_default(),
        "Created": config["created"].as_str().unwrap_or(super::model::ZERO_TIME),
        "DockerVersion": config["docker_version"].as_str().unwrap_or_default(),
        "Author": config["author"].as_str().unwrap_or_default(),
        "Config": image_config,
        "Architecture": config["architecture"].as_str().unwrap_or_default(),
        "Variant": config["variant"].clone(),
        "Os": config["os"].as_str().unwrap_or("linux"),
        "Size": size,
        "VirtualSize": size,
        "GraphDriver": {"Name": "overlay2", "Data": null},
        "RootFS": {"Type": "layers", "Layers": info.entry.layers},
        "Metadata": {"LastTagTime": super::model::ZERO_TIME},
    })
}

/// `GET /images/{name}/history`: from the config's history, newest first.
pub fn history(store: &ImageStore, info: &ImageInfo) -> Value {
    let config = store.config_blob(&info.entry.config).unwrap_or(Value::Null);
    let mut out = Vec::new();
    let mut layers = info.entry.layers.iter().rev();
    let hist: Vec<Value> = config["history"].as_array().cloned().unwrap_or_default();
    for (i, h) in hist.iter().rev().enumerate() {
        let empty = h["empty_layer"].as_bool().unwrap_or(false);
        let size = if empty { 0 } else { layers.next().map(|l| store.layer_size(l)).unwrap_or(0) };
        out.push(json!({
            "Id": if i == 0 { info.id.clone() } else { "<missing>".into() },
            "Created": h["created"].as_str().and_then(super::model::parse_time).map(|t| t.timestamp()).unwrap_or(0),
            "CreatedBy": h["created_by"].as_str().unwrap_or_default(),
            "Tags": if i == 0 { json!(info.tags()) } else { Value::Null },
            "Size": size,
            "Comment": h["comment"].as_str().unwrap_or_default(),
        }));
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(short_name("docker.io/library/nginx:latest").as_deref(), Some("nginx:latest"));
        assert_eq!(short_name("docker.io/bitnami/redis:7").as_deref(), Some("bitnami/redis:7"));
        assert_eq!(short_name("ghcr.io/a/b:1").as_deref(), Some("ghcr.io/a/b:1"));
        assert_eq!(short_name("sha256:abc"), None);
        assert_eq!(key_for("nginx").as_deref(), Some("docker.io/library/nginx:latest"));
        assert_eq!(strip_tag("localhost:5000/x:1"), "localhost:5000/x");
        assert_eq!(strip_tag("localhost:5000/x"), "localhost:5000/x");
    }
}
