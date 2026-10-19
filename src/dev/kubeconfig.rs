//! Merging the dev cluster's context into the user's kubeconfig, the way
//! kind and k3d do: other clusters, contexts and users are kept as they are.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::{Mapping, Value};

pub const CONTEXT: &str = "litek8s-dev";

/// The kubeconfig file kubectl writes to: the first entry of $KUBECONFIG,
/// else ~/.kube/config.
pub fn default_path() -> PathBuf {
    if let Some(first) = std::env::var_os("KUBECONFIG").and_then(|v| std::env::split_paths(&v).next()) {
        if !first.as_os_str().is_empty() {
            return first;
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".kube").join("config")
}

fn seq<'a>(doc: &'a mut Mapping, key: &str) -> &'a mut Vec<Value> {
    let entry = doc.entry(Value::from(key)).or_insert_with(|| Value::Sequence(Vec::new()));
    if !entry.is_sequence() {
        *entry = Value::Sequence(Vec::new());
    }
    entry.as_sequence_mut().unwrap()
}

fn upsert(list: &mut Vec<Value>, name: &str, key: &str, body: Value) {
    let mut item = Mapping::new();
    item.insert("name".into(), name.into());
    item.insert(key.into(), body);
    let item = Value::Mapping(item);
    match list.iter_mut().find(|v| v.get("name").and_then(Value::as_str) == Some(name)) {
        Some(existing) => *existing = item,
        None => list.push(item),
    }
}

/// `doc` with the dev cluster, user and context set (pointing at `server`)
/// and, if `make_current`, made the current context.
pub fn merge(doc: &str, server: &str, make_current: bool) -> Result<String> {
    let mut root = if doc.trim().is_empty() {
        Mapping::new()
    } else {
        match serde_yaml::from_str::<Value>(doc).context("the existing kubeconfig is not valid YAML")? {
            Value::Mapping(m) => m,
            Value::Null => Mapping::new(),
            _ => anyhow::bail!("the existing kubeconfig is not a YAML mapping"),
        }
    };
    root.entry("apiVersion".into()).or_insert("v1".into());
    root.entry("kind".into()).or_insert("Config".into());
    let mut cluster = Mapping::new();
    cluster.insert("server".into(), server.into());
    upsert(seq(&mut root, "clusters"), CONTEXT, "cluster", Value::Mapping(cluster));
    upsert(seq(&mut root, "users"), CONTEXT, "user", Value::Mapping(Mapping::new()));
    let mut context = Mapping::new();
    context.insert("cluster".into(), CONTEXT.into());
    context.insert("user".into(), CONTEXT.into());
    upsert(seq(&mut root, "contexts"), CONTEXT, "context", Value::Mapping(context));
    if make_current || root.get("current-context").and_then(Value::as_str).is_none_or(str::is_empty) {
        root.insert("current-context".into(), CONTEXT.into());
    }
    Ok(serde_yaml::to_string(&Value::Mapping(root))?)
}

/// `doc` without the dev cluster's entries; the current context is cleared
/// if it was the dev one.
pub fn remove(doc: &str) -> Result<String> {
    let Value::Mapping(mut root) = serde_yaml::from_str::<Value>(doc).unwrap_or(Value::Null) else {
        return Ok(doc.to_string());
    };
    for key in ["clusters", "users", "contexts"] {
        if let Some(Value::Sequence(list)) = root.get_mut(key) {
            list.retain(|v| v.get("name").and_then(Value::as_str) != Some(CONTEXT));
        }
    }
    if root.get("current-context").and_then(Value::as_str) == Some(CONTEXT) {
        root.insert("current-context".into(), "".into());
    }
    Ok(serde_yaml::to_string(&Value::Mapping(root))?)
}

fn write_private(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("litek8s-{}.tmp", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| format!("failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

/// Add or update the dev context in the kubeconfig at `path`.
pub fn install(path: &Path, server: &str) -> Result<()> {
    let doc = std::fs::read_to_string(path).unwrap_or_default();
    write_private(path, &merge(&doc, server, true)?)
}

/// `doc` with `context` (which must exist) as the current context.
pub fn with_current(doc: &str, context: &str) -> Result<String> {
    let Value::Mapping(mut root) = serde_yaml::from_str::<Value>(doc).context("the kubeconfig is not valid YAML")? else {
        anyhow::bail!("the kubeconfig is not a YAML mapping");
    };
    let exists = root.get("contexts").and_then(Value::as_sequence).is_some_and(|l| l.iter().any(|c| c.get("name").and_then(Value::as_str) == Some(context)));
    if !exists {
        anyhow::bail!("there is no context {context:?} in the kubeconfig");
    }
    root.insert("current-context".into(), context.into());
    Ok(serde_yaml::to_string(&Value::Mapping(root))?)
}

/// Make `context` the current context of the kubeconfig at `path`.
pub fn set_current(path: &Path, context: &str) -> Result<()> {
    let doc = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    write_private(path, &with_current(&doc, context)?)
}

/// The current context of the kubeconfig at `path` and its server.
pub fn current(path: &Path) -> Option<(String, Option<String>)> {
    let root: Value = serde_yaml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let name = root.get("current-context")?.as_str()?.to_string();
    let cluster = root.get("contexts")?.as_sequence()?.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(&name))?.get("context")?.get("cluster")?.as_str()?.to_string();
    let server = root
        .get("clusters")
        .and_then(Value::as_sequence)
        .and_then(|l| l.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(&cluster)))
        .and_then(|c| c.get("cluster")?.get("server")?.as_str().map(str::to_string));
    Some((name, server))
}

/// Drop the dev context from the kubeconfig at `path`.
pub fn uninstall(path: &Path) -> Result<()> {
    let Ok(doc) = std::fs::read_to_string(path) else { return Ok(()) };
    if !doc.contains(CONTEXT) {
        return Ok(());
    }
    write_private(path, &remove(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND: &str = "apiVersion: v1\nkind: Config\nclusters:\n- name: kind-kind\n  cluster:\n    server: https://127.0.0.1:41234\n    certificate-authority-data: AAAA\ncontexts:\n- name: kind-kind\n  context:\n    cluster: kind-kind\n    user: kind-kind\ncurrent-context: kind-kind\nusers:\n- name: kind-kind\n  user:\n    client-key-data: BBBB\npreferences: {}\n";

    #[test]
    fn merges_next_to_other_clusters_and_removes_cleanly() {
        let merged = merge(KIND, "http://127.0.0.1:6443", true).unwrap();
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["current-context"], "litek8s-dev");
        assert_eq!(v["clusters"].as_sequence().unwrap().len(), 2);
        assert_eq!(v["clusters"][1]["cluster"]["server"], "http://127.0.0.1:6443");
        assert_eq!(v["users"][0]["user"]["client-key-data"], "BBBB");
        assert!(v.get("preferences").is_some());
        // Idempotent: a second merge updates in place.
        let again = merge(&merged, "http://127.0.0.1:7443", true).unwrap();
        let v2: Value = serde_yaml::from_str(&again).unwrap();
        assert_eq!(v2["contexts"].as_sequence().unwrap().len(), 2);
        assert_eq!(v2["clusters"][1]["cluster"]["server"], "http://127.0.0.1:7443");

        let removed: Value = serde_yaml::from_str(&remove(&again).unwrap()).unwrap();
        assert_eq!(removed["clusters"].as_sequence().unwrap().len(), 1);
        assert_eq!(removed["current-context"], "");
    }

    #[test]
    fn switches_the_current_context() {
        let merged = merge(KIND, "http://127.0.0.1:6443", false).unwrap();
        let v: Value = serde_yaml::from_str(&with_current(&merged, CONTEXT).unwrap()).unwrap();
        assert_eq!(v["current-context"], "litek8s-dev");
        assert_eq!(v["users"][0]["user"]["client-key-data"], "BBBB");
        assert!(with_current(KIND, "nope").is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, &merged).unwrap();
        assert_eq!(current(&path), Some(("kind-kind".to_string(), Some("https://127.0.0.1:41234".to_string()))));
        set_current(&path, CONTEXT).unwrap();
        assert_eq!(current(&path), Some((CONTEXT.to_string(), Some("http://127.0.0.1:6443".to_string()))));
    }

    #[test]
    fn creates_a_kubeconfig_from_nothing() {
        let v: Value = serde_yaml::from_str(&merge("", "http://127.0.0.1:6443", false).unwrap()).unwrap();
        assert_eq!(v["kind"], "Config");
        assert_eq!(v["current-context"], "litek8s-dev");
        let kept: Value = serde_yaml::from_str(&merge(KIND, "http://x", false).unwrap()).unwrap();
        assert_eq!(kept["current-context"], "kind-kind");
    }
}
