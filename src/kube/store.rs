//! Generic object store behind the Kubernetes API.
//!
//! Every object is kept as JSON in memory, keyed by (group, resource,
//! namespace, name), with one cluster-wide `resourceVersion` counter. Each
//! mutation appends a record to an event history (for watches that resume from
//! a resourceVersion) and is broadcast to live watchers. With a data directory
//! the store is durable through an append-only write-ahead log that is
//! compacted to one record per object on startup and when it grows large;
//! nothing is rewritten in place, which matters on SD cards and slow disks.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// How many past events a watch can resume from before getting `410 Gone`.
const HISTORY_CAPACITY: usize = 10_000;
/// Compact the WAL once it holds this many records beyond one per object.
const WAL_COMPACT_SLACK: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectKey {
    pub group: String,
    pub resource: String,
    /// Empty for cluster-scoped objects.
    pub namespace: String,
    pub name: String,
}

impl ObjectKey {
    pub fn new(group: &str, resource: &str, namespace: &str, name: &str) -> Self {
        Self {
            group: group.to_string(),
            resource: resource.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Added,
    Modified,
    Deleted,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::Added => "ADDED",
            EventType::Modified => "MODIFIED",
            EventType::Deleted => "DELETED",
        }
    }
}

#[derive(Debug)]
pub struct WatchEvent {
    pub rv: u64,
    pub key: ObjectKey,
    pub event: EventType,
    pub object: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    AlreadyExists,
    /// The caller's resourceVersion precondition did not match.
    Conflict,
    /// A watch asked to resume from a resourceVersion older than the history.
    Gone,
}

#[derive(Serialize, Deserialize)]
struct WalRecord {
    rv: u64,
    key: ObjectKey,
    /// `None` is a delete.
    obj: Option<Value>,
}

struct Wal {
    path: PathBuf,
    writer: BufWriter<File>,
    records: usize,
}

struct Inner {
    objects: BTreeMap<ObjectKey, Value>,
    rv: u64,
    history: VecDeque<Arc<WatchEvent>>,
    wal: Option<Wal>,
}

pub struct Store {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Arc<WatchEvent>>,
}

/// One watch subscription: the backlog to replay first, then the live feed.
pub struct Subscription {
    pub backlog: Vec<Arc<WatchEvent>>,
    pub live: broadcast::Receiver<Arc<WatchEvent>>,
}

impl Store {
    /// Open a store. `None` keeps everything in memory only.
    pub fn open(data_dir: Option<&Path>) -> Result<Self> {
        let mut objects = BTreeMap::new();
        let mut rv = 0;
        let wal = match data_dir {
            None => None,
            Some(dir) => {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("failed to create {}", dir.display()))?;
                let path = dir.join("objects.wal");
                if path.exists() {
                    let file = File::open(&path)
                        .with_context(|| format!("failed to open {}", path.display()))?;
                    for (n, line) in BufReader::new(file).lines().enumerate() {
                        let line = line?;
                        if line.trim().is_empty() {
                            continue;
                        }
                        // A torn final line (crash mid-append) is dropped.
                        let Ok(record) = serde_json::from_str::<WalRecord>(&line) else {
                            eprintln!("apiserver: ignoring unreadable WAL record at line {}", n + 1);
                            continue;
                        };
                        rv = rv.max(record.rv);
                        match record.obj {
                            Some(obj) => objects.insert(record.key, obj),
                            None => objects.remove(&record.key),
                        };
                    }
                }
                Some(write_compacted(&path, &objects, rv)?)
            }
        };
        let (events, _) = broadcast::channel(4096);
        Ok(Self {
            inner: Mutex::new(Inner {
                objects,
                rv,
                history: VecDeque::new(),
                wal,
            }),
            events,
        })
    }

    pub fn current_rv(&self) -> u64 {
        self.inner.lock().unwrap().rv
    }

    pub fn get(&self, key: &ObjectKey) -> Option<Value> {
        self.inner.lock().unwrap().objects.get(key).cloned()
    }

    /// All objects of one resource, optionally restricted to a namespace, in
    /// key order, plus the resourceVersion the snapshot was taken at.
    pub fn list(&self, group: &str, resource: &str, namespace: Option<&str>) -> (Vec<Value>, u64) {
        let inner = self.inner.lock().unwrap();
        let items = inner
            .objects
            .range(range_start(group, resource, namespace)..)
            .take_while(|(k, _)| {
                k.group == group
                    && k.resource == resource
                    && namespace.is_none_or(|ns| k.namespace == ns)
            })
            .map(|(_, v)| v.clone())
            .collect();
        (items, inner.rv)
    }

    /// Every key in a namespace, across all resources (namespace deletion).
    pub fn keys_in_namespace(&self, namespace: &str) -> Vec<ObjectKey> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .keys()
            .filter(|k| k.namespace == namespace)
            .cloned()
            .collect()
    }

    pub fn keys_of_resource(&self, group: &str, resource: &str) -> Vec<ObjectKey> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .range(range_start(group, resource, None)..)
            .take_while(|(k, _)| k.group == group && k.resource == resource)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn create(&self, key: ObjectKey, object: Value) -> Result<Value, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.objects.contains_key(&key) {
            return Err(StoreError::AlreadyExists);
        }
        Ok(self.commit(&mut inner, key, Some(object), EventType::Added))
    }

    /// Replace an object. `expected_rv` is the caller's resourceVersion
    /// precondition; `None` means an unconditional update.
    pub fn update(
        &self,
        key: ObjectKey,
        object: Value,
        expected_rv: Option<u64>,
    ) -> Result<Value, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(current) = inner.objects.get(&key) else {
            return Err(StoreError::NotFound);
        };
        if let Some(expected) = expected_rv {
            if resource_version(current) != Some(expected) {
                return Err(StoreError::Conflict);
            }
        }
        Ok(self.commit(&mut inner, key, Some(object), EventType::Modified))
    }

    pub fn delete(&self, key: &ObjectKey, expected_rv: Option<u64>) -> Result<Value, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(current) = inner.objects.get(key) else {
            return Err(StoreError::NotFound);
        };
        if let Some(expected) = expected_rv {
            if resource_version(current) != Some(expected) {
                return Err(StoreError::Conflict);
            }
        }
        Ok(self.commit(&mut inner, key.clone(), None, EventType::Deleted))
    }

    /// Subscribe to changes after `since`. With `None`, only future events are
    /// delivered (the caller sends its own initial list). Taken under the store
    /// lock, so no event can fall between the backlog and the live feed.
    pub fn subscribe(&self, since: Option<u64>) -> Result<Subscription, StoreError> {
        let inner = self.inner.lock().unwrap();
        let live = self.events.subscribe();
        let backlog = match since {
            None => Vec::new(),
            Some(rv) if rv >= inner.rv => Vec::new(),
            Some(rv) => {
                let oldest = inner.history.front().map(|e| e.rv).unwrap_or(inner.rv + 1);
                if rv + 1 < oldest {
                    return Err(StoreError::Gone);
                }
                inner
                    .history
                    .iter()
                    .filter(|e| e.rv > rv)
                    .cloned()
                    .collect()
            }
        };
        Ok(Subscription { backlog, live })
    }

    /// Apply one mutation: bump the resourceVersion, stamp it on the object,
    /// log it, record it in the history and broadcast it. Returns the object
    /// as stored (for a delete, the last state with the delete's rv).
    fn commit(
        &self,
        inner: &mut Inner,
        key: ObjectKey,
        object: Option<Value>,
        event: EventType,
    ) -> Value {
        inner.rv += 1;
        let rv = inner.rv;
        let mut stored = match object {
            Some(obj) => obj,
            None => inner.objects.remove(&key).unwrap_or(Value::Null),
        };
        if let Some(meta) = stored.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("resourceVersion".into(), Value::String(rv.to_string()));
        }
        if event != EventType::Deleted {
            inner.objects.insert(key.clone(), stored.clone());
        }

        let object_count = inner.objects.len();
        let snapshot_rv = inner.rv;
        if let Some(wal) = inner.wal.as_mut() {
            let record = WalRecord {
                rv,
                key: key.clone(),
                obj: (event != EventType::Deleted).then(|| stored.clone()),
            };
            let appended = serde_json::to_writer(&mut wal.writer, &record)
                .map_err(anyhow::Error::from)
                .and_then(|_| wal.writer.write_all(b"\n").map_err(Into::into))
                .and_then(|_| wal.writer.flush().map_err(Into::into));
            if let Err(error) = appended {
                eprintln!("apiserver: failed to append to WAL: {error}");
            }
            wal.records += 1;
            if wal.records > object_count + WAL_COMPACT_SLACK {
                let path = wal.path.clone();
                match write_compacted(&path, &inner.objects, snapshot_rv) {
                    Ok(fresh) => inner.wal = Some(fresh),
                    Err(error) => eprintln!("apiserver: WAL compaction failed: {error}"),
                }
            }
        }

        let event = Arc::new(WatchEvent {
            rv,
            key,
            event,
            object: stored.clone(),
        });
        inner.history.push_back(event.clone());
        if inner.history.len() > HISTORY_CAPACITY {
            inner.history.pop_front();
        }
        // No receivers is fine; watchers come and go.
        let _ = self.events.send(event);
        stored
    }
}

fn range_start(group: &str, resource: &str, namespace: Option<&str>) -> ObjectKey {
    ObjectKey::new(group, resource, namespace.unwrap_or(""), "")
}

pub fn resource_version(object: &Value) -> Option<u64> {
    object
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Rewrite the WAL as one record per live object (via a temp file + rename)
/// and return an appender positioned at its end.
fn write_compacted(path: &Path, objects: &BTreeMap<ObjectKey, Value>, rv: u64) -> Result<Wal> {
    let tmp = path.with_extension("wal.tmp");
    {
        let mut out = BufWriter::new(
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?,
        );
        for (key, obj) in objects {
            let record_rv = resource_version(obj).unwrap_or(rv);
            serde_json::to_writer(
                &mut out,
                &WalRecord {
                    rv: record_rv,
                    key: key.clone(),
                    obj: Some(obj.clone()),
                },
            )?;
            out.write_all(b"\n")?;
        }
        // Keep the counter even when the newest change was a delete.
        if objects.values().filter_map(resource_version).max() != Some(rv) && rv > 0 {
            serde_json::to_writer(
                &mut out,
                &WalRecord {
                    rv,
                    key: ObjectKey::new("", "", "", ""),
                    obj: None,
                },
            )?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    let file = OpenOptions::new().append(true).open(path)?;
    Ok(Wal {
        path: path.to_path_buf(),
        writer: BufWriter::new(file),
        records: objects.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cm(name: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "default"}})
    }

    fn key(name: &str) -> ObjectKey {
        ObjectKey::new("", "configmaps", "default", name)
    }

    #[test]
    fn crud_and_resource_versions() {
        let store = Store::open(None).unwrap();
        let a = store.create(key("a"), cm("a")).unwrap();
        assert_eq!(resource_version(&a), Some(1));
        assert_eq!(store.create(key("a"), cm("a")), Err(StoreError::AlreadyExists));
        assert_eq!(store.update(key("a"), cm("a"), Some(7)), Err(StoreError::Conflict));
        let a2 = store.update(key("a"), cm("a"), Some(1)).unwrap();
        assert_eq!(resource_version(&a2), Some(2));
        store.create(key("b"), cm("b")).unwrap();
        let (items, rv) = store.list("", "configmaps", Some("default"));
        assert_eq!((items.len(), rv), (2, 3));
        store.delete(&key("a"), None).unwrap();
        assert_eq!(store.get(&key("a")), None);
        assert_eq!(store.delete(&key("a"), None), Err(StoreError::NotFound));
    }

    #[test]
    fn subscribe_replays_only_newer_events() {
        let store = Store::open(None).unwrap();
        store.create(key("a"), cm("a")).unwrap();
        store.create(key("b"), cm("b")).unwrap();
        let sub = store.subscribe(Some(1)).unwrap();
        let names: Vec<_> = sub.backlog.iter().map(|e| e.key.name.clone()).collect();
        assert_eq!(names, vec!["b"]);
        assert!(store.subscribe(Some(2)).unwrap().backlog.is_empty());
    }

    #[test]
    fn wal_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(Some(dir.path())).unwrap();
            store.create(key("a"), cm("a")).unwrap();
            store.create(key("b"), cm("b")).unwrap();
            store.delete(&key("b"), None).unwrap();
        }
        let store = Store::open(Some(dir.path())).unwrap();
        assert!(store.get(&key("a")).is_some());
        assert!(store.get(&key("b")).is_none());
        assert_eq!(store.current_rv(), 3);
        let c = store.create(key("c"), cm("c")).unwrap();
        assert_eq!(resource_version(&c), Some(4));
    }
}
