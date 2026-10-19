//! The replicated state machine: the object store, fed by the Raft log.
//!
//! Applying an entry applies its [`Mutation`](crate::kube::store::Mutation)
//! to the in-memory [`Store`] (preconditions are checked there, so every
//! server reaches the same result and the same resourceVersion). Proposals
//! carry a (session, seq) id; outcomes are remembered until the proposer
//! acknowledges them, so a proposal retried after a lost answer (the leader
//! died after committing) is applied once.
//!
//! Snapshots replace the single-server store's WAL compaction: a file with a
//! header line (Raft metadata, resourceVersion, sessions) followed by one
//! WAL record per object, in `snapshots/snap-<index>-<term>.snap`. The state
//! machine itself is not persisted: on start it loads the newest snapshot
//! and Raft replays the log after it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine, Snapshot, SnapshotMeta};
use openraft::{EntryPayload, ErrorSubject, ErrorVerb, LogId, StorageError, StorageIOError, StoredMembership};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{Applied, Command, ServerNode, TypeConfig};
use crate::kube::store::{self, ObjectKey, Store, StoreError};

type NodeId = u64;

/// Forget a session nobody proposed from for this many entries (a
/// restarted server's old session).
const SESSION_TTL: u64 = 1_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum Outcome {
    Ok { key: ObjectKey, rv: u64 },
    Err(StoreError),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Session {
    last_index: u64,
    done: BTreeMap<u64, Outcome>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Sessions(BTreeMap<u64, Session>);

#[derive(Serialize, Deserialize)]
struct Header {
    meta: SnapshotMeta<NodeId, ServerNode>,
    rv: u64,
    sessions: Sessions,
}

fn snap_err(verb: ErrorVerb, e: impl std::error::Error + 'static) -> StorageError<NodeId> {
    StorageIOError::new(ErrorSubject::Snapshot(None), verb, &e).into()
}

pub struct StateMachine {
    store: Arc<Store>,
    dir: PathBuf,
    applied: Option<LogId<NodeId>>,
    membership: StoredMembership<NodeId, ServerNode>,
    sessions: Sessions,
    /// The file the snapshot being received is written to.
    incoming: Option<PathBuf>,
}

fn snapshot_name(meta: &SnapshotMeta<NodeId, ServerNode>) -> String {
    let (term, index) = meta.last_log_id.map(|l| (l.leader_id.term, l.index)).unwrap_or_default();
    format!("snap-{index:020}-{term:010}.snap")
}

/// The newest snapshot file in `dir`.
fn newest_snapshot(dir: &Path) -> Option<PathBuf> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("snap-") && n.ends_with(".snap"))
        .collect();
    names.sort();
    names.pop().map(|n| dir.join(n))
}

fn read_header(reader: &mut impl BufRead) -> std::io::Result<Header> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Read a whole snapshot: its header and objects.
fn read_snapshot(file: File) -> std::io::Result<(Header, Vec<(ObjectKey, Value)>)> {
    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;
    let mut objects = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let (key, _, obj) = store::parse_wal_line(&line).ok_or_else(|| std::io::Error::other("unreadable snapshot record"))?;
        if let Some(obj) = obj {
            objects.push((key, obj));
        }
    }
    Ok((header, objects))
}

/// Write a snapshot file atomically and drop older ones.
fn write_snapshot(dir: &Path, header: &Header, objects: &[(ObjectKey, u64, Arc<[u8]>)]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = snapshot_name(&header.meta);
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut out = BufWriter::new(File::create(&tmp)?);
        serde_json::to_writer(&mut out, header).map_err(std::io::Error::other)?;
        out.write_all(b"\n")?;
        for (key, rv, json) in objects {
            out.write_all(&store::wal_line(*rv, key, Some(json)))?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
    }
    let path = dir.join(&name);
    std::fs::rename(&tmp, &path)?;
    File::open(dir)?.sync_all()?;
    prune(dir, &name);
    Ok(path)
}

/// Remove snapshots older than `keep` and leftover temp files. An open
/// file being sent to a follower stays readable after unlink.
fn prune(dir: &Path, keep: &str) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let old_snap = n.starts_with("snap-") && n.ends_with(".snap") && n.as_str() < keep;
            let stale_tmp = n.ends_with(".tmp") || n.starts_with("incoming-");
            if old_snap || (stale_tmp && n != keep) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

async fn open_for_read(path: &Path) -> Result<tokio::fs::File, StorageError<NodeId>> {
    tokio::fs::File::open(path).await.map_err(|e| snap_err(ErrorVerb::Read, e))
}

impl StateMachine {
    /// Open the state machine kept in `dir`, loading the newest snapshot into `store`.
    pub fn open(dir: &Path, store: Arc<Store>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut sm = Self {
            store,
            dir: dir.to_path_buf(),
            applied: None,
            membership: StoredMembership::default(),
            sessions: Sessions::default(),
            incoming: None,
        };
        if let Some(path) = newest_snapshot(dir) {
            let (header, objects) = read_snapshot(File::open(&path)?)?;
            sm.store.restore(objects, header.rv);
            sm.applied = header.meta.last_log_id;
            sm.membership = header.meta.last_membership.clone();
            sm.sessions = header.sessions;
        }
        Ok(sm)
    }

    /// Seed a fresh state machine (a single server becoming the first
    /// member of a cluster): the store's current objects become the
    /// snapshot at log index 0 in the dir, so they survive restarts.
    pub fn seed(dir: &Path, store: &Store, membership: StoredMembership<NodeId, ServerNode>) -> anyhow::Result<()> {
        let (objects, rv) = store.export();
        let header = Header {
            meta: SnapshotMeta { last_log_id: None, last_membership: membership, snapshot_id: "seed".into() },
            rv,
            sessions: Sessions::default(),
        };
        write_snapshot(dir, &header, &objects)?;
        Ok(())
    }

    fn apply_command(&mut self, index: u64, cmd: Command) -> Result<Value, StoreError> {
        let session = self.sessions.0.entry(cmd.session).or_default();
        session.last_index = index;
        // Answered and acknowledged: the proposer stopped waiting long ago.
        session.done = session.done.split_off(&cmd.acked);
        let cached = session.done.get(&cmd.seq).cloned();
        let result = match cached {
            Some(Outcome::Ok { key, rv }) => self
                .store
                .lookup_version(&key, rv)
                .ok_or_else(|| StoreError::Unavailable("applied, but the result is no longer available".into())),
            Some(Outcome::Err(e)) => Err(e),
            None => {
                let result = self.store.apply(&cmd.mutation);
                let outcome = match &result {
                    Ok(obj) => Outcome::Ok { key: cmd.mutation.key.clone(), rv: store::resource_version(obj).unwrap_or(0) },
                    Err(e) => Outcome::Err(e.clone()),
                };
                if let Some(session) = self.sessions.0.get_mut(&cmd.session) {
                    session.done.insert(cmd.seq, outcome);
                }
                result
            }
        };
        if index % 4096 == 0 {
            self.sessions.0.retain(|_, s| s.last_index + SESSION_TTL > index);
        }
        result
    }
}

pub struct Builder {
    dir: PathBuf,
    header: Header,
    objects: Vec<(ObjectKey, u64, Arc<[u8]>)>,
}

impl RaftSnapshotBuilder<TypeConfig> for Builder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let dir = self.dir.clone();
        let header = Header {
            meta: self.header.meta.clone(),
            rv: self.header.rv,
            sessions: std::mem::take(&mut self.header.sessions),
        };
        let objects = std::mem::take(&mut self.objects);
        let meta = header.meta.clone();
        let path = tokio::task::spawn_blocking(move || write_snapshot(&dir, &header, &objects))
            .await
            .map_err(|e| snap_err(ErrorVerb::Write, std::io::Error::other(e.to_string())))?
            .map_err(|e| snap_err(ErrorVerb::Write, e))?;
        Ok(Snapshot { meta, snapshot: Box::new(open_for_read(&path).await?) })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = Builder;

    async fn applied_state(&mut self) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, ServerNode>), StorageError<NodeId>> {
        Ok((self.applied, self.membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Applied>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut out = Vec::new();
        for entry in entries {
            self.applied = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => out.push(Applied(None)),
                EntryPayload::Membership(m) => {
                    self.membership = StoredMembership::new(Some(entry.log_id), m);
                    out.push(Applied(None));
                }
                EntryPayload::Normal(cmd) => {
                    let result = self.apply_command(entry.log_id.index, cmd);
                    out.push(Applied(Some(result)));
                }
            }
        }
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        let (objects, rv) = self.store.export();
        let id = format!(
            "{}-{}-{:x}",
            self.applied.map(|l| l.leader_id.term).unwrap_or_default(),
            self.applied.map(|l| l.index).unwrap_or_default(),
            rand_u64()
        );
        Builder {
            dir: self.dir.clone(),
            header: Header {
                meta: SnapshotMeta { last_log_id: self.applied, last_membership: self.membership.clone(), snapshot_id: id },
                rv,
                sessions: self.sessions.clone(),
            },
            objects,
        }
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<tokio::fs::File>, StorageError<NodeId>> {
        let path = self.dir.join(format!("incoming-{:x}.snap", rand_u64()));
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .map_err(|e| snap_err(ErrorVerb::Write, e))?;
        self.incoming = Some(path);
        Ok(Box::new(file))
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta<NodeId, ServerNode>, snapshot: Box<tokio::fs::File>) -> Result<(), StorageError<NodeId>> {
        let mut file = snapshot.into_std().await;
        file.flush().map_err(|e| snap_err(ErrorVerb::Read, e))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| snap_err(ErrorVerb::Read, e))?;
        // Whether the data is the file begin_receiving_snapshot made.
        let inode = |m: std::io::Result<std::fs::Metadata>| m.ok().map(|m| (m.dev(), m.ino()));
        let received = self.incoming.take().filter(|p| inode(file.metadata()).is_some() && inode(file.metadata()) == inode(std::fs::metadata(p)));
        let (header, objects) = tokio::task::spawn_blocking(move || read_snapshot(file))
            .await
            .map_err(|e| snap_err(ErrorVerb::Read, std::io::Error::other(e.to_string())))?
            .map_err(|e| snap_err(ErrorVerb::Read, e))?;
        self.store.restore(objects, header.rv);
        self.applied = meta.last_log_id;
        self.membership = meta.last_membership.clone();
        self.sessions = header.sessions;
        // Keep it as the current snapshot: the received file as is, when
        // it is the one begin_receiving_snapshot made.
        let name = snapshot_name(meta);
        let kept = match received {
            Some(path) => std::fs::rename(&path, self.dir.join(&name)).is_ok(),
            None => false,
        };
        if kept {
            let _ = File::open(self.dir.join(&name)).and_then(|f| f.sync_all());
            prune(&self.dir, &name);
        } else {
            let (objects, rv) = self.store.export();
            let header = Header { meta: meta.clone(), rv, sessions: self.sessions.clone() };
            write_snapshot(&self.dir, &header, &objects).map_err(|e| snap_err(ErrorVerb::Write, e))?;
        }
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let Some(path) = newest_snapshot(&self.dir) else { return Ok(None) };
        let header = {
            let file = File::open(&path).map_err(|e| snap_err(ErrorVerb::Read, e))?;
            read_header(&mut BufReader::new(file)).map_err(|e| snap_err(ErrorVerb::Read, e))?
        };
        // The seed is not a Raft snapshot (no log id); Raft never asks for it.
        if header.meta.last_log_id.is_none() {
            return Ok(None);
        }
        Ok(Some(Snapshot { meta: header.meta, snapshot: Box::new(open_for_read(&path).await?) }))
    }
}

pub(crate) fn rand_u64() -> u64 {
    use rand_core::RngCore;
    rand_core::OsRng.next_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::store::{Mutation, MutationKind};
    use openraft::{CommittedLeaderId, Entry};
    use serde_json::json;

    fn cmd(session: u64, seq: u64, kind: MutationKind, name: &str, expected: Option<u64>, value: Option<i64>) -> Command {
        let object = value.map(|v| {
            serde_json::value::to_raw_value(&json!({"metadata": {"name": name, "namespace": "default"}, "data": {"v": v.to_string()}})).unwrap()
        });
        Command {
            session,
            seq,
            acked: 0,
            mutation: Mutation { kind, key: ObjectKey::new("", "configmaps", "default", name), expected_rv: expected, object },
        }
    }

    fn entries(cmds: Vec<Command>, from: u64) -> Vec<Entry<TypeConfig>> {
        cmds.into_iter()
            .enumerate()
            .map(|(i, c)| Entry { log_id: LogId::new(CommittedLeaderId::new(1, 1), from + i as u64), payload: EntryPayload::Normal(c) })
            .collect()
    }

    fn fingerprint(store: &Store) -> (Vec<(ObjectKey, u64, Vec<u8>)>, u64) {
        let (objs, rv) = store.export();
        (objs.into_iter().map(|(k, r, j)| (k, r, j.to_vec())).collect(), rv)
    }

    /// A deterministic pseudo-random workload.
    fn workload(seed: u64, n: usize) -> Vec<Command> {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        (0..n)
            .map(|i| {
                let r = next();
                let name = format!("k{}", r % 5);
                let session = r % 3;
                let seq = (i as u64) / 2; // duplicates on purpose
                match (r >> 8) % 4 {
                    0 => cmd(session, seq, MutationKind::Create, &name, None, Some(i as i64)),
                    1 => cmd(session, seq, MutationKind::Update, &name, None, Some(i as i64)),
                    2 => cmd(session, seq, MutationKind::Update, &name, Some((r >> 16) % 8), Some(i as i64)),
                    _ => cmd(session, seq, MutationKind::Delete, &name, None, None),
                }
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replicas_agree_and_snapshots_roundtrip() {
        for seed in 1..20u64 {
            let cmds = workload(seed, 300);
            let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let (sa, sb) = (Arc::new(Store::open(None).unwrap()), Arc::new(Store::open(None).unwrap()));
            let mut a = StateMachine::open(da.path(), sa.clone()).unwrap();
            let mut b = StateMachine::open(db.path(), sb.clone()).unwrap();
            // A applies everything in one go; B in batches with a snapshot
            // install from A in the middle.
            let all = entries(cmds.clone(), 1);
            let ra = a.apply(all.clone()).await.unwrap();
            let rb1 = b.apply(all[..150].to_vec()).await.unwrap();
            assert_eq!(format!("{:?}", &ra[..150]), format!("{:?}", rb1), "seed {seed}");
            let mut builder = {
                let mut mid = StateMachine::open(tempfile::tempdir().unwrap().path(), Arc::new(Store::open(None).unwrap())).unwrap();
                mid.apply(all[..200].to_vec()).await.unwrap();
                mid.get_snapshot_builder().await
            };
            let snap = builder.build_snapshot().await.unwrap();
            let mut incoming = b.begin_receiving_snapshot().await.unwrap();
            let mut src = snap.snapshot;
            tokio::io::copy(&mut *src, &mut *incoming).await.unwrap();
            b.install_snapshot(&snap.meta, incoming).await.unwrap();
            let rb2 = b.apply(all[200..].to_vec()).await.unwrap();
            assert_eq!(format!("{:?}", &ra[200..]), format!("{:?}", rb2), "seed {seed}");
            assert_eq!(fingerprint(&sa), fingerprint(&sb), "seed {seed}");
            assert_eq!(a.sessions, b.sessions);
            // Reopening from the installed snapshot and replaying the rest
            // gives the same state again.
            drop(b);
            let sc = Arc::new(Store::open(None).unwrap());
            let mut c = StateMachine::open(db.path(), sc.clone()).unwrap();
            assert_eq!(c.applied.map(|l| l.index), Some(200));
            c.apply(all[200..].to_vec()).await.unwrap();
            assert_eq!(fingerprint(&sa), fingerprint(&sc), "seed {seed}");
        }
    }

    struct SuiteBuilder;

    impl openraft::testing::StoreBuilder<TypeConfig, crate::kube::ha::log::LogStore, StateMachine, tempfile::TempDir> for SuiteBuilder {
        async fn build(&self) -> Result<(tempfile::TempDir, crate::kube::ha::log::LogStore, StateMachine), StorageError<NodeId>> {
            let dir = tempfile::tempdir().unwrap();
            let log = crate::kube::ha::log::LogStore::open(&dir.path().join("log")).unwrap();
            let sm = StateMachine::open(&dir.path().join("snapshots"), Arc::new(Store::open(None).unwrap())).unwrap();
            Ok((dir, log, sm))
        }
    }

    /// openraft's own conformance suite for log and state machine storage.
    #[test]
    fn openraft_storage_suite() {
        openraft::testing::Suite::test_all(SuiteBuilder).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retried_proposals_apply_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(None).unwrap());
        let mut sm = StateMachine::open(dir.path(), store.clone()).unwrap();
        let create = cmd(7, 1, MutationKind::Create, "a", None, Some(1));
        let update = cmd(7, 2, MutationKind::Update, "a", None, Some(2));
        let r = sm.apply(entries(vec![create.clone(), update.clone(), update.clone(), create.clone()], 1)).await.unwrap();
        let rvs: Vec<_> = r.iter().map(|a| a.0.clone().unwrap().map(|v| store::resource_version(&v))).collect();
        assert_eq!(rvs, vec![Ok(Some(1)), Ok(Some(2)), Ok(Some(2)), Ok(Some(1))]);
        assert_eq!(store.current_rv(), 2);
        // Once acknowledged, the outcome is forgotten.
        let mut later = cmd(7, 3, MutationKind::Delete, "a", None, None);
        later.acked = 3;
        sm.apply(entries(vec![later], 5)).await.unwrap();
        assert_eq!(sm.sessions.0[&7].done.keys().copied().collect::<Vec<_>>(), vec![3]);
    }
}
