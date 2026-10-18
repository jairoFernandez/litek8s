//! Pod volumes on the node.
//!
//! - `emptyDir` and persistent volumes are runtime volumes (they live where
//!   the containers run, keep ownership semantics, and work the same on a
//!   Linux host and on a VM-backed Docker); `medium: Memory` is a tmpfs.
//! - `configMap`, `secret`, `downwardAPI` and `projected` (which carries the
//!   service account token) are files written under the agent's directory
//!   and bind-mounted read-only.
//! - `hostPath` is a bind mount.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::Value;

use super::Agent;
use super::runtime::{LABEL_POD_UID, PodRuntime};
use crate::kube::quantity;

#[derive(Debug, Clone)]
pub(crate) enum Prepared {
    /// Files on the node, bind-mounted.
    Dir(PathBuf),
    /// A runtime volume; `fresh` when just created (needs `chmod 0777`).
    Named { volume: String, fresh: bool, empty_dir: bool },
    Tmpfs,
    HostPath(String),
}

pub(crate) fn pv_volume_name(pv: &str) -> String {
    format!("litek8s-pv-{pv}")
}

pub(crate) fn empty_dir_volume(uid: &str, name: &str) -> String {
    format!("litek8s-ed-{}-{name}", &uid[..8.min(uid.len())])
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// A downward API `fieldRef` value.
pub(crate) fn field_ref(pod: &Value, path: &str, pod_ip: &str, host_ip: &str) -> Option<String> {
    let map_lines = |ptr: &str| {
        let mut lines: Vec<String> = pod
            .pointer(ptr)
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}=\"{}\"", v.as_str().unwrap_or_default().replace('"', "\\\"")))
                    .collect()
            })
            .unwrap_or_default();
        lines.sort();
        lines.join("\n")
    };
    let keyed = |prefix: &str, ptr: &str| {
        let key = path.strip_prefix(prefix)?.strip_suffix("']")?;
        Some(
            pod.pointer(ptr)
                .and_then(|m| m.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };
    Some(match path {
        "metadata.name" => s(pod, "/metadata/name").to_string(),
        "metadata.namespace" => s(pod, "/metadata/namespace").to_string(),
        "metadata.uid" => s(pod, "/metadata/uid").to_string(),
        "metadata.labels" => map_lines("/metadata/labels"),
        "metadata.annotations" => map_lines("/metadata/annotations"),
        "spec.nodeName" => s(pod, "/spec/nodeName").to_string(),
        "spec.serviceAccountName" => s(pod, "/spec/serviceAccountName").to_string(),
        "status.podIP" | "status.podIPs" => pod_ip.to_string(),
        "status.hostIP" | "status.hostIPs" => host_ip.to_string(),
        p if p.starts_with("metadata.labels['") => keyed("metadata.labels['", "/metadata/labels")?,
        p if p.starts_with("metadata.annotations['") => keyed("metadata.annotations['", "/metadata/annotations")?,
        _ => return None,
    })
}

/// A downward API `resourceFieldRef` value for `container` (limits fall
/// back to the node's allocatable, as in Kubernetes).
pub(crate) fn resource_ref(pod: &Value, container_name: &str, resource: &str, divisor: Option<&Value>, node: &Value) -> String {
    let container = ["/spec/containers", "/spec/initContainers"]
        .iter()
        .filter_map(|p| pod.pointer(p).and_then(Value::as_array))
        .flatten()
        .find(|c| c.get("name").and_then(Value::as_str) == Some(container_name).filter(|n| !n.is_empty()))
        .or_else(|| pod.pointer("/spec/containers/0"));
    let (kind, res) = resource.split_once('.').unwrap_or(("limits", resource));
    let value = container
        .and_then(|c| c.pointer(&format!("/resources/{kind}/{res}")))
        .and_then(quantity::milli_of)
        .or_else(|| node.pointer(&format!("/status/allocatable/{res}")).and_then(quantity::milli_of))
        .unwrap_or(0);
    let divisor = divisor.and_then(quantity::milli_of).filter(|d| *d > 0).unwrap_or(1000);
    // Round up, as the kubelet does.
    ((value + divisor - 1) / divisor).to_string()
}

fn write_file(dir: &Path, rel: &str, data: &[u8], mode: u32) -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    if rel.split('/').any(|seg| seg == "..") {
        return Err(format!("invalid volume item path {rel}"));
    }
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, data).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777));
    Ok(())
}

/// The key → bytes of a ConfigMap or Secret.
fn object_data(obj: &Value, secret: bool) -> BTreeMap<String, Vec<u8>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = BTreeMap::new();
    if let Some(data) = obj.get("data").and_then(Value::as_object) {
        for (k, v) in data {
            let raw = v.as_str().unwrap_or_default();
            let bytes = if secret { b64.decode(raw).unwrap_or_else(|_| raw.as_bytes().to_vec()) } else { raw.as_bytes().to_vec() };
            out.insert(k.clone(), bytes);
        }
    }
    if let Some(data) = obj.get("binaryData").and_then(Value::as_object) {
        for (k, v) in data {
            if let Ok(bytes) = b64.decode(v.as_str().unwrap_or_default()) {
                out.insert(k.clone(), bytes);
            }
        }
    }
    out
}

/// Write a ConfigMap/Secret projection (all keys, or `items`).
fn project(dir: &Path, data: &BTreeMap<String, Vec<u8>>, items: Option<&Value>, mode: u32, optional: bool, what: &str) -> Result<(), String> {
    match items.and_then(Value::as_array) {
        Some(items) => {
            for item in items {
                let key = s(item, "/key");
                let path = item.get("path").and_then(Value::as_str).unwrap_or(key);
                let item_mode = item.get("mode").and_then(Value::as_u64).map(|m| m as u32).unwrap_or(mode);
                match data.get(key) {
                    Some(bytes) => write_file(dir, path, bytes, item_mode)?,
                    None if optional => {}
                    None => return Err(format!("couldn't find key {key} in {what}")),
                }
            }
        }
        None => {
            for (key, bytes) in data {
                write_file(dir, key, bytes, mode)?;
            }
        }
    }
    Ok(())
}

impl<R: PodRuntime> Agent<R> {
    async fn lookup(&self, resource: &str, ns: &str, name: &str) -> Result<Option<Value>, String> {
        let link = self.link().ok_or("not connected to the API server")?;
        link.get(resource, ns, name).await.map_err(|e| format!("{e:#}"))
    }

    /// Make every volume of `pod` available; errors read like the kubelet's
    /// (they end up in the pod's waiting reason).
    pub(crate) async fn prepare_volumes(&self, pod: &Value, pod_dir: &Path, node: &Value) -> Result<HashMap<String, Prepared>, String> {
        let ns = s(pod, "/metadata/namespace");
        let uid = s(pod, "/metadata/uid");
        let fs_group = pod.pointer("/spec/securityContext/fsGroup").and_then(Value::as_u64).map(|g| g as u32);
        let mut out = HashMap::new();
        for volume in pod.pointer("/spec/volumes").and_then(Value::as_array).into_iter().flatten() {
            let name = s(volume, "/name").to_string();
            let dir = pod_dir.join("volumes").join(&name);
            let default_mode = |src: &Value| src.get("defaultMode").and_then(Value::as_u64).map(|m| m as u32).unwrap_or(0o644);
            let prepared = if let Some(ed) = volume.get("emptyDir") {
                if s(ed, "/medium") == "Memory" {
                    Prepared::Tmpfs
                } else {
                    let v = empty_dir_volume(uid, &name);
                    let fresh = self
                        .runtime()
                        .ensure_volume(&v, HashMap::from([(LABEL_POD_UID.to_string(), uid.to_string()), (super::runtime::LABEL_NODE.to_string(), self.config().node_name.clone())]))
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    Prepared::Named { volume: v, fresh, empty_dir: true }
                }
            } else if let Some(claim) = volume.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str) {
                let pvc = self
                    .lookup("persistentvolumeclaims", ns, claim)
                    .await?
                    .ok_or_else(|| format!("persistentvolumeclaim \"{claim}\" not found"))?;
                let pv = s(&pvc, "/spec/volumeName");
                if pv.is_empty() {
                    return Err(format!("persistentvolumeclaim \"{claim}\" is not bound"));
                }
                let pv_obj = self.lookup("persistentvolumes", "", pv).await?;
                match pv_obj.as_ref().and_then(|p| p.pointer("/spec/hostPath/path")).and_then(Value::as_str) {
                    Some(path) => Prepared::HostPath(path.to_string()),
                    None => {
                        let v = pv_volume_name(pv);
                        let fresh = self
                            .runtime()
                            .ensure_volume(&v, HashMap::from([("litek8s.pv".to_string(), pv.to_string())]))
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                        Prepared::Named { volume: v, fresh, empty_dir: false }
                    }
                }
            } else if let Some(path) = volume.pointer("/hostPath/path").and_then(Value::as_str) {
                Prepared::HostPath(path.to_string())
            } else if let Some(src) = volume.get("configMap") {
                let stage = begin_write(&dir)?;
                let cm_name = s(src, "/name");
                let optional = src.get("optional").and_then(Value::as_bool) == Some(true);
                match self.lookup("configmaps", ns, cm_name).await? {
                    Some(cm) => project(&stage, &object_data(&cm, false), src.get("items"), default_mode(src), optional, &format!("configmap {cm_name}"))?,
                    None if optional => {}
                    None => return Err(format!("configmap \"{cm_name}\" not found")),
                }
                publish(&dir, &stage, fs_group)?;
                Prepared::Dir(dir)
            } else if let Some(src) = volume.get("secret") {
                let stage = begin_write(&dir)?;
                let secret_name = s(src, "/secretName");
                let optional = src.get("optional").and_then(Value::as_bool) == Some(true);
                match self.lookup("secrets", ns, secret_name).await? {
                    Some(sec) => project(&stage, &object_data(&sec, true), src.get("items"), default_mode(src), optional, &format!("secret {secret_name}"))?,
                    None if optional => {}
                    None => return Err(format!("secret \"{secret_name}\" not found")),
                }
                publish(&dir, &stage, fs_group)?;
                Prepared::Dir(dir)
            } else if let Some(src) = volume.get("downwardAPI") {
                let stage = begin_write(&dir)?;
                self.write_downward(&stage, pod, src.get("items"), default_mode(src), node)?;
                publish(&dir, &stage, fs_group)?;
                Prepared::Dir(dir)
            } else if let Some(src) = volume.get("projected") {
                let stage = begin_write(&dir)?;
                let mode = default_mode(src);
                for source in src.get("sources").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(cm) = source.get("configMap") {
                        let cm_name = s(cm, "/name");
                        let optional = cm.get("optional").and_then(Value::as_bool) == Some(true);
                        match self.lookup("configmaps", ns, cm_name).await? {
                            Some(obj) => project(&stage, &object_data(&obj, false), cm.get("items"), mode, optional, &format!("configmap {cm_name}"))?,
                            None if optional => {}
                            None => return Err(format!("configmap \"{cm_name}\" not found")),
                        }
                    } else if let Some(sec) = source.get("secret") {
                        let sec_name = s(sec, "/name");
                        let optional = sec.get("optional").and_then(Value::as_bool) == Some(true);
                        match self.lookup("secrets", ns, sec_name).await? {
                            Some(obj) => project(&stage, &object_data(&obj, true), sec.get("items"), mode, optional, &format!("secret {sec_name}"))?,
                            None if optional => {}
                            None => return Err(format!("secret \"{sec_name}\" not found")),
                        }
                    } else if let Some(da) = source.get("downwardAPI") {
                        self.write_downward(&stage, pod, da.get("items"), mode, node)?;
                    } else if let Some(tok) = source.get("serviceAccountToken") {
                        let sa = s(pod, "/spec/serviceAccountName");
                        let request = serde_json::json!({
                            "sa": if sa.is_empty() { "default" } else { sa },
                            "pod": s(pod, "/metadata/name"),
                            "audience": s(tok, "/audience"),
                            "expirationSeconds": tok.get("expirationSeconds").and_then(Value::as_i64),
                        })
                        .to_string();
                        let token = self
                            .lookup("token", ns, &request)
                            .await?
                            .and_then(|t| t.get("token").and_then(Value::as_str).map(str::to_string))
                            .ok_or("failed to get a service account token")?;
                        let path = tok.get("path").and_then(Value::as_str).unwrap_or("token");
                        write_file(&stage, path, token.as_bytes(), mode)?;
                    }
                }
                publish(&dir, &stage, fs_group)?;
                Prepared::Dir(dir)
            } else {
                // csi, ephemeral and the rest: scratch space.
                let v = empty_dir_volume(uid, &name);
                let fresh = self
                    .runtime()
                    .ensure_volume(&v, HashMap::from([(LABEL_POD_UID.to_string(), uid.to_string()), (super::runtime::LABEL_NODE.to_string(), self.config().node_name.clone())]))
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                Prepared::Named { volume: v, fresh, empty_dir: true }
            };
            out.insert(name, prepared);
        }
        Ok(out)
    }

    fn write_downward(&self, dir: &Path, pod: &Value, items: Option<&Value>, mode: u32, node: &Value) -> Result<(), String> {
        let host_ip = &self.config().node_ip;
        for item in items.and_then(Value::as_array).into_iter().flatten() {
            let path = s(item, "/path");
            let value = if let Some(f) = item.pointer("/fieldRef/fieldPath").and_then(Value::as_str) {
                field_ref(pod, f, "", host_ip).unwrap_or_default()
            } else if let Some(r) = item.get("resourceFieldRef") {
                resource_ref(pod, s(r, "/containerName"), s(r, "/resource"), r.get("divisor"), node)
            } else {
                continue;
            };
            let item_mode = item.get("mode").and_then(Value::as_u64).map(|m| m as u32).unwrap_or(mode);
            write_file(dir, path, value.as_bytes(), item_mode)?;
        }
        Ok(())
    }
}

/// Start writing a new version of a projected volume: a fresh hidden
/// directory next to the current one (the kubelet's atomic writer layout).
fn begin_write(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let stamp = chrono::Utc::now().format("..%Y_%m_%d_%H_%M_%S.%f").to_string();
    let stage = dir.join(stamp);
    std::fs::create_dir_all(&stage).map_err(|e| format!("failed to create {}: {e}", stage.display()))?;
    Ok(stage)
}

/// Switch the volume to `stage` atomically: `..data` points at it and every
/// top-level entry is a symlink through `..data`, as the kubelet lays it
/// out (some charts look for those symlinks). Containers keep the volume
/// directory bind-mounted, so they see the switch.
fn publish(dir: &Path, stage: &Path, fs_group: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    let io = |e: std::io::Error| format!("failed to update volume {}: {e}", dir.display());
    if let Some(gid) = fs_group {
        apply_fs_group(stage, gid);
    }
    let _ = std::fs::set_permissions(stage, std::fs::Permissions::from_mode(0o755));
    let stage_name = stage.file_name().unwrap_or_default().to_string_lossy().to_string();
    let entries: Vec<String> = std::fs::read_dir(stage)
        .map_err(io)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let tmp = dir.join("..data_tmp");
    let _ = std::fs::remove_file(&tmp);
    symlink(&stage_name, &tmp).map_err(io)?;
    std::fs::rename(&tmp, dir.join("..data")).map_err(io)?;
    for name in &entries {
        let link = dir.join(name);
        match std::fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => continue,
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&link).map_err(io)?,
            Ok(_) => std::fs::remove_file(&link).map_err(io)?,
            Err(_) => {}
        }
        symlink(format!("..data/{name}"), &link).map_err(io)?;
    }
    // Drop entries and versions that are no longer current.
    for entry in std::fs::read_dir(dir).map_err(io)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name == "..data" || name == stage_name || entries.contains(&name) {
            continue;
        }
        if name.starts_with("..") && path.is_dir() && !path.is_symlink() {
            let _ = std::fs::remove_dir_all(&path);
        } else if path.is_symlink() || !name.starts_with("..") {
            let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir_all(&path));
        }
    }
    Ok(())
}

/// Give the pod's `fsGroup` access the way the kubelet does (group
/// ownership). Where the agent may not change ownership (not root), the
/// group's permissions are granted to owner and others as well.
fn apply_fs_group(dir: &Path, gid: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if std::os::unix::fs::chown(&path, None, Some(gid)).is_err() {
            if let Ok(meta) = std::fs::metadata(&path) {
                let mode = meta.permissions().mode() & 0o777;
                // Owner too: VM-backed Docker shows bind-mounted files as
                // owned by whoever reads them (so there, owner bits suffice).
                let group = (mode >> 3) & 0o7;
                let widened = if cfg!(target_os = "linux") { mode | (group << 6) | group } else { mode | (group << 6) };
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(widened));
            }
        }
        if path.is_dir() {
            apply_fs_group(&path, gid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publishes_like_the_kubelet_atomic_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("vol");
        let stage = begin_write(&dir).unwrap();
        write_file(&stage, "a", b"1", 0o644).unwrap();
        write_file(&stage, "old", b"x", 0o644).unwrap();
        publish(&dir, &stage, None).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("a")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "1");
        let stage = begin_write(&dir).unwrap();
        write_file(&stage, "a", b"2", 0o644).unwrap();
        publish(&dir, &stage, None).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "2");
        assert!(!dir.join("old").exists() && std::fs::symlink_metadata(dir.join("old")).is_err());
        let versions = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.path().is_dir() && !e.path().is_symlink()).count();
        assert_eq!(versions, 1, "older versions are removed");
    }

    #[test]
    fn downward_fields() {
        let pod = json!({"metadata": {"name": "p", "namespace": "n", "labels": {"b": "2", "a": "1"}}, "spec": {"nodeName": "x"}});
        assert_eq!(field_ref(&pod, "metadata.name", "", "").as_deref(), Some("p"));
        assert_eq!(field_ref(&pod, "metadata.labels", "", "").as_deref(), Some("a=\"1\"\nb=\"2\""));
        assert_eq!(field_ref(&pod, "metadata.labels['b']", "", "").as_deref(), Some("2"));
        assert_eq!(field_ref(&pod, "status.podIP", "10.0.0.1", "").as_deref(), Some("10.0.0.1"));
        assert_eq!(field_ref(&pod, "spec.bogus", "", ""), None);
    }

    #[test]
    fn resource_refs_round_up() {
        let pod = json!({"spec": {"containers": [{"name": "c", "resources": {"limits": {"cpu": "250m", "memory": "64Mi"}}}]}});
        let node = json!({"status": {"allocatable": {"cpu": "4", "memory": "8Gi"}}});
        assert_eq!(resource_ref(&pod, "c", "limits.cpu", None, &node), "1");
        assert_eq!(resource_ref(&pod, "c", "limits.memory", None, &node), (64 * 1024 * 1024).to_string());
        assert_eq!(resource_ref(&pod, "c", "limits.memory", Some(&json!("1Mi")), &node), "64");
        assert_eq!(resource_ref(&pod, "c", "requests.cpu", Some(&json!("1m")), &node), "4000");
    }
}

/// Whether `pod` mounts a ConfigMap or Secret named `name` as files.
pub(crate) fn uses_object(pod: &Value, resource: &str, name: &str) -> bool {
    let (field, key) = match resource {
        "configmaps" => ("configMap", "name"),
        "secrets" => ("secret", "secretName"),
        _ => return false,
    };
    pod.pointer("/spec/volumes").and_then(Value::as_array).into_iter().flatten().any(|v| {
        v.get(field).and_then(|src| src.get(key)).and_then(Value::as_str) == Some(name)
            || v.pointer("/projected/sources").and_then(Value::as_array).is_some_and(|sources| {
                sources.iter().any(|src| src.get(field).and_then(|x| x.get("name")).and_then(Value::as_str) == Some(name))
            })
    })
}
