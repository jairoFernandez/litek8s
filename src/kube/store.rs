//! Generic object store behind the Kubernetes API.
//!
//! Every object is kept in memory as compact JSON bytes (parsed on read: a
//! parsed `serde_json::Value` takes ~10x the space, which on a 1 GiB node is
//! the difference between 15 and 150 MB for a few charts' CRDs), keyed by
//! (group, resource, namespace, name), with one cluster-wide
//! `resourceVersion` counter. Each
//! mutation appends a record to an event history (for watches that resume from
//! a resourceVersion) and is broadcast to live watchers. With a data directory
//! the store is durable through an append-only write-ahead log that is
//! compacted to one record per object on startup and when it grows large;
//! nothing is rewritten in place, which matters on SD cards and slow disks.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// How many past events a watch can resume from before getting `410 Gone`
/// (clients then relist). History keeps each version as compact JSON bytes,
/// a quarter of the size of the parsed form.
const HISTORY_CAPACITY: usize = 4096;
/// ... and at most this many bytes: a burst of large objects (a chart's
/// CRDs are up to ~1 MB each) must not pin hundreds of MB.
const HISTORY_MAX_BYTES: usize = 16 << 20;
/// Live change-feed buffer. Every consumer drains it right away; one that
/// falls this far behind resyncs. Kept small because each slot pins a parsed
/// object version until it is overwritten.
const FEED_CAPACITY: usize = 256;
/// Compact the WAL once it holds this many records beyond one per object.
const WAL_COMPACT_SLACK: usize = 20_000;
/// ... or once it is this much larger than right after the last compaction
/// (records of big objects add up long before the record count does).
const WAL_COMPACT_BYTES: u64 = 64 << 20;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreError {
    NotFound,
    AlreadyExists,
    /// The caller's resourceVersion precondition did not match.
    Conflict,
    /// A watch asked to resume from a resourceVersion older than the history.
    Gone,
    /// A replicated store could not commit the change (no leader, no
    /// quorum, timed out); it may or may not have been applied.
    Unavailable(String),
}

/// What a mutation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationKind {
    Create,
    Update,
    Delete,
}

/// One store mutation, self-contained: its preconditions are checked when
/// it is applied. In HA mode this is what the Raft log carries, so every
/// server applies the same mutations in the same order and assigns the same
/// resourceVersions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutation {
    pub kind: MutationKind,
    pub key: ObjectKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_rv: Option<u64>,
    /// The object to store (create/update), as compact JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<Box<serde_json::value::RawValue>>,
}

/// Takes the store's mutations elsewhere (the Raft log) instead of applying
/// them here; the replicated state machine applies them back with
/// [`Store::apply`]. Blocks until the mutation is applied locally.
pub trait Replicator: Send + Sync {
    fn replicate(&self, mutation: Mutation) -> Result<Value, StoreError>;
}

/// A past change, kept for watches that resume from a resourceVersion.
#[derive(Clone)]
struct HistoryEntry {
    rv: u64,
    key: ObjectKey,
    event: EventType,
    /// Shared with the object map while this is the current version.
    object: Arc<[u8]>,
}

impl HistoryEntry {
    fn to_event(&self) -> Arc<WatchEvent> {
        Arc::new(WatchEvent {
            rv: self.rv,
            key: self.key.clone(),
            event: self.event,
            object: serde_json::from_slice(&self.object).unwrap_or(Value::Null),
        })
    }
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
    bytes: u64,
    /// Size right after the last compaction.
    base_bytes: u64,
}

/// Sizes the store keeps, for `/metrics`.
#[derive(Debug, Clone, Default)]
pub struct StoreStats {
    pub objects: usize,
    pub resource_version: u64,
    pub wal_records: usize,
    pub wal_bytes: u64,
    pub compactions: u64,
    pub history_entries: usize,
    pub history_bytes: usize,
    pub feed_receivers: usize,
}

/// An object as stored: its resourceVersion and its compact JSON.
#[derive(Clone)]
struct Stored {
    rv: u64,
    json: Arc<[u8]>,
}

impl Stored {
    fn new(obj: &Value) -> Self {
        Self { rv: resource_version(obj).unwrap_or(0), json: serde_json::to_vec(obj).unwrap_or_default().into() }
    }

    fn value(&self) -> Value {
        serde_json::from_slice(&self.json).unwrap_or(Value::Null)
    }
}

struct Inner {
    objects: BTreeMap<ObjectKey, Stored>,
    rv: u64,
    history: VecDeque<HistoryEntry>,
    history_bytes: usize,
    wal: Option<Wal>,
    compactions: u64,
    /// The live change feed. Replaced (ending every subscription) when a
    /// snapshot replaces the whole state.
    events: broadcast::Sender<Arc<WatchEvent>>,
}

pub struct Store {
    inner: Mutex<Inner>,
    /// Set in HA mode: mutations go through the Raft log.
    replicator: OnceLock<Arc<dyn Replicator>>,
    /// Woken on every commit (waits for a resourceVersion).
    committed: tokio::sync::Notify,
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
                            Some(obj) => objects.insert(record.key, Stored::new(&obj)),
                            None => objects.remove(&record.key),
                        };
                    }
                }
                Some(write_compacted(&path, &objects, rv)?)
            }
        };
        let (events, _) = broadcast::channel(FEED_CAPACITY);
        Ok(Self {
            inner: Mutex::new(Inner {
                objects,
                rv,
                history: VecDeque::new(),
                history_bytes: 0,
                wal,
                compactions: 0,
                events,
            }),
            replicator: OnceLock::new(),
            committed: tokio::sync::Notify::new(),
        })
    }

    /// Send every mutation through `replicator` from now on (HA mode).
    pub fn set_replicator(&self, replicator: Arc<dyn Replicator>) {
        let _ = self.replicator.set(replicator);
    }

    pub fn is_replicated(&self) -> bool {
        self.replicator.get().is_some()
    }

    /// Objects of a WAL file (one record per line, as written by a
    /// single-server store), for moving a single server to HA.
    pub fn read_wal(path: &Path) -> Result<(Vec<(ObjectKey, Value)>, u64)> {
        let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut objects = BTreeMap::new();
        let mut rv = 0;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let Ok(record) = serde_json::from_str::<WalRecord>(&line) else { continue };
            rv = rv.max(record.rv);
            match record.obj {
                Some(obj) => objects.insert(record.key, obj),
                None => objects.remove(&record.key),
            };
        }
        Ok((objects.into_iter().collect(), rv))
    }

    pub fn stats(&self) -> StoreStats {
        let inner = self.inner.lock().unwrap();
        StoreStats {
            objects: inner.objects.len(),
            resource_version: inner.rv,
            wal_records: inner.wal.as_ref().map_or(0, |w| w.records),
            wal_bytes: inner.wal.as_ref().map_or(0, |w| w.bytes),
            compactions: inner.compactions,
            history_entries: inner.history.len(),
            history_bytes: inner.history_bytes,
            feed_receivers: inner.events.receiver_count(),
        }
    }

    pub fn current_rv(&self) -> u64 {
        self.inner.lock().unwrap().rv
    }

    pub fn get(&self, key: &ObjectKey) -> Option<Value> {
        // Copy the bytes out, parse outside the lock.
        let json = self.inner.lock().unwrap().objects.get(key).map(|s| s.json.clone());
        json.map(|j| serde_json::from_slice(&j).unwrap_or(Value::Null))
    }

    /// The version of `key` written at `rv`, while the object or the history
    /// still has it (a deduplicated retry's answer).
    pub fn lookup_version(&self, key: &ObjectKey, rv: u64) -> Option<Value> {
        let json = {
            let inner = self.inner.lock().unwrap();
            match inner.objects.get(key).filter(|s| s.rv == rv) {
                Some(s) => Some(s.json.clone()),
                None => inner.history.iter().rev().find(|e| e.rv == rv && &e.key == key).map(|e| e.object.clone()),
            }
        };
        json.map(|j| serde_json::from_slice(&j).unwrap_or(Value::Null))
    }

    /// An object's resourceVersion, without copying the object.
    pub fn resource_version_of(&self, key: &ObjectKey) -> Option<u64> {
        self.inner.lock().unwrap().objects.get(key).map(|s| s.rv)
    }

    /// All objects of one resource, optionally restricted to a namespace, in
    /// key order, plus the resourceVersion the snapshot was taken at.
    pub fn list(&self, group: &str, resource: &str, namespace: Option<&str>) -> (Vec<Value>, u64) {
        let (jsons, rv): (Vec<Arc<[u8]>>, u64) = {
            let inner = self.inner.lock().unwrap();
            let jsons = inner
                .objects
                .range(range_start(group, resource, namespace)..)
                .take_while(|(k, _)| {
                    k.group == group
                        && k.resource == resource
                        && namespace.is_none_or(|ns| k.namespace == ns)
                })
                .map(|(_, v)| v.json.clone())
                .collect();
            (jsons, inner.rv)
        };
        (jsons.iter().map(|j| serde_json::from_slice(j).unwrap_or(Value::Null)).collect(), rv)
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

    /// Every object in a namespace (`""`: cluster-scoped ones), any resource.
    pub fn objects_in_namespace(&self, namespace: &str) -> Vec<(ObjectKey, Value)> {
        // Copy the bytes out, parse outside the lock.
        let found: Vec<(ObjectKey, Arc<[u8]>)> = {
            let inner = self.inner.lock().unwrap();
            inner.objects.iter().filter(|(k, _)| k.namespace == namespace).map(|(k, v)| (k.clone(), v.json.clone())).collect()
        };
        found.into_iter().map(|(k, j)| (k, serde_json::from_slice(&j).unwrap_or(Value::Null))).collect()
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
        self.mutate(MutationKind::Create, key, Some(object), None)
    }

    /// Replace an object. `expected_rv` is the caller's resourceVersion
    /// precondition; `None` means an unconditional update.
    pub fn update(
        &self,
        key: ObjectKey,
        object: Value,
        expected_rv: Option<u64>,
    ) -> Result<Value, StoreError> {
        self.mutate(MutationKind::Update, key, Some(object), expected_rv)
    }

    pub fn delete(&self, key: &ObjectKey, expected_rv: Option<u64>) -> Result<Value, StoreError> {
        self.mutate(MutationKind::Delete, key.clone(), None, expected_rv)
    }

    fn mutate(&self, kind: MutationKind, key: ObjectKey, object: Option<Value>, expected_rv: Option<u64>) -> Result<Value, StoreError> {
        match self.replicator.get() {
            Some(replicator) => {
                let object = object
                    .map(|o| serde_json::value::to_raw_value(&o).map_err(|e| StoreError::Unavailable(format!("unencodable object: {e}"))))
                    .transpose()?;
                replicator.replicate(Mutation { kind, key, expected_rv, object })
            }
            None => self.apply_local(kind, key, object, expected_rv),
        }
    }

    /// Apply a replicated mutation (the state machine's side of
    /// [`Replicator`]). Deterministic: the same mutations in the same order
    /// give the same objects and resourceVersions on every server.
    pub fn apply(&self, mutation: &Mutation) -> Result<Value, StoreError> {
        let object = match &mutation.object {
            Some(raw) => Some(serde_json::from_str(raw.get()).unwrap_or(Value::Null)),
            None => None,
        };
        self.apply_local(mutation.kind, mutation.key.clone(), object, mutation.expected_rv)
    }

    fn apply_local(&self, kind: MutationKind, key: ObjectKey, object: Option<Value>, expected_rv: Option<u64>) -> Result<Value, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let result = match kind {
            MutationKind::Create => {
                if inner.objects.contains_key(&key) {
                    return Err(StoreError::AlreadyExists);
                }
                self.commit(&mut inner, key, Some(object.unwrap_or(Value::Null)), EventType::Added)
            }
            MutationKind::Update | MutationKind::Delete => {
                let Some(current) = inner.objects.get(&key) else {
                    return Err(StoreError::NotFound);
                };
                if expected_rv.is_some_and(|expected| current.rv != expected) {
                    return Err(StoreError::Conflict);
                }
                if kind == MutationKind::Update {
                    self.commit(&mut inner, key, Some(object.unwrap_or(Value::Null)), EventType::Modified)
                } else {
                    self.commit(&mut inner, key, None, EventType::Deleted)
                }
            }
        };
        drop(inner);
        self.committed.notify_waiters();
        Ok(result)
    }

    /// Wait until the store has reached resourceVersion `rv` (a replica
    /// catching up), for at most `timeout`. Returns whether it did.
    pub async fn wait_for_rv(&self, rv: u64, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.committed.notified();
            if self.current_rv() >= rv {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.current_rv() >= rv;
            }
        }
    }

    /// The whole state, for a snapshot: every object's key, resourceVersion
    /// and JSON (shared, not copied), and the store's resourceVersion.
    pub fn export(&self) -> (Vec<(ObjectKey, u64, Arc<[u8]>)>, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.objects.iter().map(|(k, v)| (k.clone(), v.rv, v.json.clone())).collect(), inner.rv)
    }

    /// Replace the whole state (a snapshot was installed). The history is
    /// dropped and every live subscription ends: watchers resume from a
    /// resourceVersion the history no longer has, get `410 Gone` and re-list.
    pub fn restore(&self, objects: Vec<(ObjectKey, Value)>, rv: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.objects = objects.into_iter().map(|(k, v)| (k, Stored::new(&v))).collect();
        inner.rv = rv;
        inner.history.clear();
        inner.history_bytes = 0;
        inner.events = broadcast::channel(FEED_CAPACITY).0;
        drop(inner);
        self.committed.notify_waiters();
    }

    /// Subscribe to changes after `since`. With `None`, only future events are
    /// delivered (the caller sends its own initial list). Taken under the store
    /// lock, so no event can fall between the backlog and the live feed.
    pub fn subscribe(&self, since: Option<u64>) -> Result<Subscription, StoreError> {
        self.subscribe_for(since, |_| true)
    }

    /// [`subscribe`](Self::subscribe) with a backlog of only the events for
    /// keys `wanted` accepts (a watch replays one resource, not the whole
    /// history; the live feed is unfiltered).
    pub fn subscribe_for(&self, since: Option<u64>, wanted: impl Fn(&ObjectKey) -> bool) -> Result<Subscription, StoreError> {
        let (live, entries) = {
            let inner = self.inner.lock().unwrap();
            let live = inner.events.subscribe();
            let entries: Vec<HistoryEntry> = match since {
                None => Vec::new(),
                Some(rv) if rv >= inner.rv => Vec::new(),
                Some(rv) => {
                    let oldest = inner.history.front().map(|e| e.rv).unwrap_or(inner.rv + 1);
                    if rv + 1 < oldest {
                        return Err(StoreError::Gone);
                    }
                    inner.history.iter().filter(|e| e.rv > rv && wanted(&e.key)).cloned().collect()
                }
            };
            (live, entries)
        };
        // Parsed outside the lock.
        let backlog = entries.iter().map(HistoryEntry::to_event).collect();
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
            None => inner.objects.remove(&key).map(|s| s.value()).unwrap_or(Value::Null),
        };
        if let Some(meta) = stored.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("resourceVersion".into(), Value::String(rv.to_string()));
        }
        // Serialized once: the object map, the history and the WAL share it.
        let json: Arc<[u8]> = serde_json::to_vec(&stored).unwrap_or_default().into();
        if event != EventType::Deleted {
            inner.objects.insert(key.clone(), Stored { rv, json: json.clone() });
        }

        let object_count = inner.objects.len();
        let snapshot_rv = inner.rv;
        if let Some(wal) = inner.wal.as_mut() {
            let line = wal_line(rv, &key, (event != EventType::Deleted).then_some(&json[..]));
            let appended = wal
                .writer
                .write_all(&line)
                .and_then(|_| wal.writer.write_all(b"\n"))
                .and_then(|_| wal.writer.flush());
            if let Err(error) = appended {
                eprintln!("apiserver: failed to append to WAL: {error}");
            }
            wal.records += 1;
            wal.bytes += line.len() as u64 + 1;
            if wal.records > object_count + WAL_COMPACT_SLACK || wal.bytes > wal.base_bytes + WAL_COMPACT_BYTES.max(wal.base_bytes * 2) {
                let path = wal.path.clone();
                match write_compacted(&path, &inner.objects, snapshot_rv) {
                    Ok(fresh) => {
                        inner.wal = Some(fresh);
                        inner.compactions += 1;
                    }
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
        inner.history_bytes += json.len();
        inner.history.push_back(HistoryEntry {
            rv,
            key: event.key.clone(),
            event: event.event,
            object: json,
        });
        while inner.history.len() > HISTORY_CAPACITY || (inner.history_bytes > HISTORY_MAX_BYTES && inner.history.len() > 1) {
            let Some(old) = inner.history.pop_front() else { break };
            inner.history_bytes -= old.object.len();
        }
        // No receivers is fine; watchers come and go.
        let _ = inner.events.send(event);
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

/// One WAL line, from the object's JSON as stored (`None`: a delete).
/// Snapshots of a replicated store use the same records.
pub(crate) fn wal_line(rv: u64, key: &ObjectKey, obj: Option<&[u8]>) -> Vec<u8> {
    let key = serde_json::to_vec(key).unwrap_or_default();
    let obj = obj.unwrap_or(b"null");
    let mut line = Vec::with_capacity(obj.len() + key.len() + 40);
    line.extend_from_slice(format!("{{\"rv\":{rv},\"key\":").as_bytes());
    line.extend_from_slice(&key);
    line.extend_from_slice(b",\"obj\":");
    line.extend_from_slice(obj);
    line.push(b'}');
    line
}

/// A WAL (or snapshot) line: key, resourceVersion and the object (`None`:
/// a delete, or the record that only carries the store's resourceVersion).
pub(crate) fn parse_wal_line(line: &str) -> Option<(ObjectKey, u64, Option<Value>)> {
    let record: WalRecord = serde_json::from_str(line).ok()?;
    Some((record.key, record.rv, record.obj))
}

/// Rewrite the WAL as one record per live object (via a temp file + rename)
/// and return an appender positioned at its end.
fn write_compacted(path: &Path, objects: &BTreeMap<ObjectKey, Stored>, rv: u64) -> Result<Wal> {
    let tmp = path.with_extension("wal.tmp");
    {
        let mut out = BufWriter::new(
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?,
        );
        for (key, obj) in objects {
            let record_rv = if obj.rv > 0 { obj.rv } else { rv };
            out.write_all(&wal_line(record_rv, key, Some(&obj.json)))?;
            out.write_all(b"\n")?;
        }
        // Keep the counter even when the newest change was a delete.
        if objects.values().map(|o| o.rv).max() != Some(rv) && rv > 0 {
            out.write_all(&wal_line(rv, &ObjectKey::new("", "", "", ""), None))?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    let file = OpenOptions::new().append(true).open(path)?;
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(Wal {
        path: path.to_path_buf(),
        writer: BufWriter::new(file),
        records: objects.len(),
        bytes,
        base_bytes: bytes,
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
        let only_a = store.subscribe_for(Some(0), |k| k.name == "a").unwrap();
        let names: Vec<_> = only_a.backlog.iter().map(|e| e.key.name.clone()).collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn history_is_bounded_by_bytes() {
        let store = Store::open(None).unwrap();
        let big = "x".repeat(1 << 20);
        for i in 0..40 {
            let mut obj = cm(&format!("c{i}"));
            obj["data"] = json!({"blob": big});
            store.create(key(&format!("c{i}")), obj).unwrap();
        }
        let st = store.stats();
        assert!(st.history_bytes <= HISTORY_MAX_BYTES, "{} bytes kept", st.history_bytes);
        assert!(st.history_entries < 40 && st.history_entries >= 15);
    }

    #[test]
    fn wal_compacts_by_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let big = "y".repeat(1 << 20);
        let mut obj = cm("big");
        obj["data"] = json!({"blob": big});
        store.create(key("big"), obj.clone()).unwrap();
        for _ in 0..100 {
            store.update(key("big"), obj.clone(), None).unwrap();
        }
        let st = store.stats();
        assert!(st.compactions >= 1, "{st:?}");
        assert!(st.wal_bytes < 80 << 20, "{st:?}");
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

/// Memory a store holding real objects takes: `LITEK8S_STORE_FIXTURE` is a
/// file of JSON objects (one per line), loaded `LITEK8S_STORE_COPIES` times.
/// `cargo test --release --lib store_memory -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn store_memory() {
    let Ok(path) = std::env::var("LITEK8S_STORE_FIXTURE") else { return };
    let copies: usize = std::env::var("LITEK8S_STORE_COPIES").ok().and_then(|c| c.parse().ok()).unwrap_or(1);
    let rss = || {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0) / 1024
    };
    let lines: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().map(str::to_string).collect();
    let bytes: usize = lines.iter().map(String::len).sum::<usize>() * copies;
    let before = rss();
    let store = Store::open(None).unwrap();
    for c in 0..copies {
        for (i, line) in lines.iter().enumerate() {
            let obj: Value = serde_json::from_str(line).unwrap();
            store.create(ObjectKey::new("x", "y", "", &format!("{c}-{i}")), obj).unwrap();
        }
    }
    let (items, _) = store.list("x", "y", None);
    drop(items);
    eprintln!("{} objects, {} MiB of JSON: {} MiB resident over the baseline", lines.len() * copies, bytes >> 20, rss() - before);
    // Steady state: the change feed has moved on to small objects.
    for i in 0..FEED_CAPACITY * 2 {
        store.create(ObjectKey::new("", "configmaps", "default", &format!("cm{i}")), serde_json::json!({"metadata": {"name": "cm"}})).unwrap();
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
    eprintln!("after {} more small changes: {} MiB", FEED_CAPACITY * 2, rss() - before);
}
