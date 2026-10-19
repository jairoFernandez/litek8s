//! The Raft log on disk.
//!
//! Entries are appended to segment files (`log-<first index>.seg`, a new one
//! every [`SEGMENT_BYTES`]), each record `[len: u32][crc32: u32][JSON entry]`.
//! Nothing is rewritten in place: a conflict truncates the tail, a purge
//! (after a snapshot) deletes whole segments. An append returns once the
//! bytes are written; a flusher thread makes them durable (one `fdatasync`
//! for every append that queued up meanwhile: group commit) and only then
//! tells Raft, which acknowledges nothing before that.
//!
//! The vote is kept in `vote.json` (fsynced), the committed index in
//! `committed.json` (not fsynced: it only saves replaying on restart) and
//! the last purged log id in `purged.json`.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::ops::{Bound, RangeBounds};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{Entry, ErrorSubject, ErrorVerb, LogId, StorageError, StorageIOError, Vote};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::types::TypeConfig;

/// Start a new segment once the current one is this large.
const SEGMENT_BYTES: u64 = 16 << 20;
const HEADER: usize = 8;

type NodeId = u64;

fn io_err(subject: ErrorSubject<NodeId>, verb: ErrorVerb, e: impl std::error::Error + 'static) -> StorageError<NodeId> {
    StorageIOError::new(subject, verb, &e).into()
}

fn log_err(e: impl std::error::Error + 'static) -> StorageError<NodeId> {
    io_err(ErrorSubject::Logs, ErrorVerb::Write, e)
}

/// CRC-32 (IEEE), for torn or corrupt records.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut crc = !0u32;
    for b in data {
        crc = table[((crc ^ u32::from(*b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Write `value` as JSON to `path` atomically (temp file, fsync, rename).
pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize, sync: bool) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let file = File::create(&tmp)?;
        serde_json::to_writer(&file, value).map_err(io::Error::other)?;
        if sync {
            file.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    if sync {
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
    }
    Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

struct Segment {
    path: PathBuf,
    file: Arc<File>,
    len: u64,
}

#[derive(Clone, Copy)]
struct Loc {
    /// First index of the segment.
    segment: u64,
    offset: u64,
    len: u32,
    log_id: LogId<NodeId>,
}

struct Inner {
    dir: PathBuf,
    segments: BTreeMap<u64, Segment>,
    index: BTreeMap<u64, Loc>,
    last_purged: Option<LogId<NodeId>>,
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    /// When `committed.json` was last written.
    committed_saved: Option<std::time::Instant>,
}

struct FlushJob {
    files: Vec<Arc<File>>,
    callback: LogFlushed<TypeConfig>,
}

/// The log storage handed to openraft.
pub struct LogStore {
    inner: Arc<Mutex<Inner>>,
    flusher: mpsc::Sender<FlushJob>,
}

/// A reader sharing the log (replication tasks read concurrently).
#[derive(Clone)]
pub struct LogReader {
    inner: Arc<Mutex<Inner>>,
}

fn segment_path(dir: &Path, first: u64) -> PathBuf {
    dir.join(format!("log-{first:020}.seg"))
}

impl LogStore {
    /// Open (or create) the log in `dir`, dropping a torn tail.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let last_purged: Option<LogId<NodeId>> = read_json(&dir.join("purged.json"))?;
        let vote: Option<Vote<NodeId>> = read_json(&dir.join("vote.json"))?;
        let committed: Option<LogId<NodeId>> = read_json(&dir.join("committed.json"))?;
        let mut firsts: Vec<u64> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_prefix("log-")?.strip_suffix(".seg")?.parse().ok()
            })
            .collect();
        firsts.sort_unstable();
        let mut segments = BTreeMap::new();
        let mut index = BTreeMap::new();
        let mut broken = false;
        for first in firsts {
            let path = segment_path(dir, first);
            if broken {
                // Everything after a corrupt record is unreachable.
                eprintln!("apiserver: raft log: dropping {} after a corrupt record", path.display());
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut data = Vec::new();
            (&file).read_to_end(&mut data)?;
            let mut pos = 0usize;
            while pos + HEADER <= data.len() {
                let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
                let Some(payload) = data.get(pos + HEADER..pos + HEADER + len) else { break };
                if crc32(payload) != crc {
                    break;
                }
                let Ok(entry) = serde_json::from_slice::<Entry<TypeConfig>>(payload) else { break };
                index.insert(entry.log_id.index, Loc { segment: first, offset: pos as u64, len: len as u32, log_id: entry.log_id });
                pos += HEADER + len;
            }
            if pos < data.len() {
                eprintln!("apiserver: raft log: truncating {} at byte {pos} of {} (torn or corrupt record)", path.display(), data.len());
                file.set_len(pos as u64)?;
                file.sync_all()?;
                broken = true;
            }
            segments.insert(first, Segment { path, file: Arc::new(file), len: pos as u64 });
        }
        if let Some(purged) = last_purged {
            index.retain(|i, _| *i > purged.index);
        }
        let inner = Arc::new(Mutex::new(Inner { dir: dir.to_path_buf(), segments, index, last_purged, vote, committed, committed_saved: None }));
        let (tx, rx) = mpsc::channel::<FlushJob>();
        std::thread::Builder::new().name("raft-log-flush".into()).spawn(move || flusher(rx))?;
        Ok(Self { inner, flusher: tx })
    }
}

impl LogStore {
    /// Write entries (not yet durable); returns the files to sync.
    fn write_entries(&self, entries: impl IntoIterator<Item = Entry<TypeConfig>>) -> Result<Vec<Arc<File>>, StorageError<NodeId>> {
        let mut files: Vec<Arc<File>> = Vec::new();
        let mut inner = self.inner.lock().unwrap();
        for entry in entries {
            let payload = serde_json::to_vec(&entry).map_err(log_err)?;
            let first = inner.active(entry.log_id.index).map_err(log_err)?;
            let seg = inner.segments.get_mut(&first).unwrap();
            let mut record = Vec::with_capacity(HEADER + payload.len());
            record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            record.extend_from_slice(&crc32(&payload).to_le_bytes());
            record.extend_from_slice(&payload);
            seg.file.write_all_at(&record, seg.len).map_err(log_err)?;
            let loc = Loc { segment: first, offset: seg.len, len: payload.len() as u32, log_id: entry.log_id };
            seg.len += record.len() as u64;
            if !files.iter().any(|f| Arc::ptr_eq(f, &seg.file)) {
                files.push(seg.file.clone());
            }
            inner.index.insert(entry.log_id.index, loc);
        }
        Ok(files)
    }
}

/// Make appends durable, as many at a time as have queued up.
fn flusher(rx: mpsc::Receiver<FlushJob>) {
    while let Ok(first) = rx.recv() {
        let mut jobs = vec![first];
        while let Ok(more) = rx.try_recv() {
            jobs.push(more);
        }
        let mut files: Vec<Arc<File>> = Vec::new();
        for job in &jobs {
            for f in &job.files {
                if !files.iter().any(|x| Arc::ptr_eq(x, f)) {
                    files.push(f.clone());
                }
            }
        }
        let mut result: io::Result<()> = Ok(());
        for f in &files {
            if let Err(e) = f.sync_data() {
                result = Err(e);
                break;
            }
        }
        for job in jobs {
            let r = match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            job.callback.log_io_completed(r);
        }
    }
}

impl Inner {
    fn read_range(&self, start: Bound<u64>, end: Bound<u64>) -> Vec<(Arc<File>, Loc)> {
        self.index
            .range((start, end))
            .map(|(_, loc)| (self.segments[&loc.segment].file.clone(), *loc))
            .collect()
    }

    /// The segment to append to, rotated when full.
    fn active(&mut self, next_index: u64) -> io::Result<u64> {
        let rotate = match self.segments.iter().next_back() {
            None => true,
            Some((_, seg)) => seg.len >= SEGMENT_BYTES,
        };
        if rotate {
            let path = segment_path(&self.dir, next_index);
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
            File::open(&self.dir)?.sync_all()?;
            self.segments.insert(next_index, Segment { path, file: Arc::new(file), len: 0 });
        }
        Ok(*self.segments.keys().next_back().unwrap())
    }
}

fn read_entries(locs: Vec<(Arc<File>, Loc)>) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
    let mut out = Vec::with_capacity(locs.len());
    for (file, loc) in locs {
        let mut buf = vec![0u8; loc.len as usize];
        file.read_exact_at(&mut buf, loc.offset + HEADER as u64)
            .map_err(|e| io_err(ErrorSubject::Log(loc.log_id), ErrorVerb::Read, e))?;
        let entry = serde_json::from_slice(&buf).map_err(|e| io_err(ErrorSubject::Log(loc.log_id), ErrorVerb::Read, e))?;
        out.push(entry);
    }
    Ok(out)
}

fn bounds<RB: RangeBounds<u64>>(range: &RB) -> (Bound<u64>, Bound<u64>) {
    (range.start_bound().cloned(), range.end_bound().cloned())
}

impl RaftLogReader<TypeConfig> for LogReader {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(&mut self, range: RB) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let (s, e) = bounds(&range);
        let locs = self.inner.lock().unwrap().read_range(s, e);
        read_entries(locs)
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(&mut self, range: RB) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let (s, e) = bounds(&range);
        let locs = self.inner.lock().unwrap().read_range(s, e);
        read_entries(locs)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogReader;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let inner = self.inner.lock().unwrap();
        let last = inner.index.values().next_back().map(|l| l.log_id).or(inner.last_purged);
        Ok(LogState { last_purged_log_id: inner.last_purged, last_log_id: last })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        LogReader { inner: self.inner.clone() }
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        write_json_atomic(&inner.dir.join("vote.json"), vote, true).map_err(|e| io_err(ErrorSubject::Vote, ErrorVerb::Write, e))?;
        inner.vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().vote)
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        inner.committed = committed;
        // Best effort, once a second: on restart it only saves waiting for
        // the leader (a stale value is a smaller one, which is safe).
        if inner.committed_saved.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
            inner.committed_saved = Some(std::time::Instant::now());
            let _ = write_json_atomic(&inner.dir.join("committed.json"), &committed, false);
        }
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().committed)
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let files = self.write_entries(entries)?;
        self.flusher
            .send(FlushJob { files, callback })
            .map_err(|_| log_err(io::Error::other("raft log flusher stopped")))?;
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(first_loc) = inner.index.range(log_id.index..).next().map(|(_, l)| *l) else { return Ok(()) };
        inner.index.retain(|i, _| *i < log_id.index);
        let doomed: Vec<u64> = inner.segments.range(first_loc.segment + 1..).map(|(k, _)| *k).collect();
        for first in doomed {
            if let Some(seg) = inner.segments.remove(&first) {
                let _ = std::fs::remove_file(&seg.path);
            }
        }
        let seg = inner.segments.get_mut(&first_loc.segment).unwrap();
        seg.file.set_len(first_loc.offset).map_err(log_err)?;
        seg.file.sync_all().map_err(log_err)?;
        seg.len = first_loc.offset;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.lock().unwrap();
        write_json_atomic(&inner.dir.join("purged.json"), &log_id, true).map_err(log_err)?;
        inner.last_purged = Some(log_id);
        inner.index.retain(|i, _| *i > log_id.index);
        // A segment goes once the next one starts at or before the first
        // entry still needed; the active (last) one always stays.
        let firsts: Vec<u64> = inner.segments.keys().copied().collect();
        for pair in firsts.windows(2) {
            if pair[1] <= log_id.index + 1 {
                if let Some(seg) = inner.segments.remove(&pair[0]) {
                    let _ = std::fs::remove_file(&seg.path);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::{CommittedLeaderId, EntryPayload};

    fn entry(term: u64, index: u64) -> Entry<TypeConfig> {
        Entry { log_id: LogId::new(CommittedLeaderId::new(term, 1), index), payload: EntryPayload::Blank }
    }

    async fn append(log: &mut LogStore, entries: Vec<Entry<TypeConfig>>) {
        for f in log.write_entries(entries).unwrap() {
            f.sync_data().unwrap();
        }
    }

    #[test]
    fn crc_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn append_truncate_purge_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut log = LogStore::open(dir.path()).unwrap();
            append(&mut log, (1..=10).map(|i| entry(1, i)).collect()).await;
            log.truncate(LogId::new(CommittedLeaderId::new(1, 1), 8)).await.unwrap();
            append(&mut log, (8..=12).map(|i| entry(2, i)).collect()).await;
            log.save_vote(&Vote::new(2, 1)).await.unwrap();
            let got = log.try_get_log_entries(5..=9).await.unwrap();
            assert_eq!(got.iter().map(|e| (e.log_id.leader_id.term, e.log_id.index)).collect::<Vec<_>>(), vec![(1, 5), (1, 6), (1, 7), (2, 8), (2, 9)]);
        }
        let mut log = LogStore::open(dir.path()).unwrap();
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.map(|l| (l.leader_id.term, l.index)), Some((2, 12)));
        assert_eq!(log.read_vote().await.unwrap(), Some(Vote::new(2, 1)));
        log.purge(LogId::new(CommittedLeaderId::new(1, 1), 6)).await.unwrap();
        assert_eq!(log.try_get_log_entries(..).await.unwrap().first().map(|e| e.log_id.index), Some(7));
        drop(log);
        let mut log = LogStore::open(dir.path()).unwrap();
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.map(|l| l.index), Some(6));
        assert_eq!(log.try_get_log_entries(..).await.unwrap().len(), 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn torn_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut log = LogStore::open(dir.path()).unwrap();
            append(&mut log, (1..=3).map(|i| entry(1, i)).collect()).await;
        }
        let seg = segment_path(dir.path(), 1);
        let len = std::fs::metadata(&seg).unwrap().len();
        let f = OpenOptions::new().write(true).open(&seg).unwrap();
        f.set_len(len - 3).unwrap();
        let mut log = LogStore::open(dir.path()).unwrap();
        assert_eq!(log.get_log_state().await.unwrap().last_log_id.map(|l| l.index), Some(2));
        append(&mut log, vec![entry(1, 3)]).await;
        drop(log);
        let mut log = LogStore::open(dir.path()).unwrap();
        assert_eq!(log.try_get_log_entries(..).await.unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn segments_rotate_and_purge_whole_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = LogStore::open(dir.path()).unwrap();
        {
            let mut inner = log.inner.lock().unwrap();
            inner.active(1).unwrap();
            // Pretend the first segment is full so the next append rotates.
        }
        append(&mut log, vec![entry(1, 1)]).await;
        log.inner.lock().unwrap().segments.get_mut(&1).unwrap().len = SEGMENT_BYTES;
        append(&mut log, vec![entry(1, 2), entry(1, 3)]).await;
        assert_eq!(log.inner.lock().unwrap().segments.len(), 2);
        log.purge(LogId::new(CommittedLeaderId::new(1, 1), 1)).await.unwrap();
        assert_eq!(log.inner.lock().unwrap().segments.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(!segment_path(dir.path(), 1).exists());
    }
}
