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
    /// Opened by the flusher on the segment's first write.
    file: Option<Arc<File>>,
    /// Bytes appended so far (written or still queued).
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

/// An appended entry the flusher has not written yet: reads take it from here.
struct Pending {
    /// Tells a re-append of the same index (after a truncate) apart.
    seq: u64,
    payload: Arc<[u8]>,
}

struct Inner {
    dir: PathBuf,
    segments: BTreeMap<u64, Segment>,
    index: BTreeMap<u64, Loc>,
    pending: BTreeMap<u64, Pending>,
    seq: u64,
    last_purged: Option<LogId<NodeId>>,
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    /// When `committed.json` was last queued.
    committed_saved: Option<std::time::Instant>,
}

enum Done {
    Raft(LogFlushed<TypeConfig>),
    #[cfg(test)]
    Test(mpsc::Sender<io::Result<()>>),
}

impl Done {
    fn complete(self, result: io::Result<()>) {
        match self {
            Done::Raft(cb) => cb.log_io_completed(result),
            #[cfg(test)]
            Done::Test(tx) => {
                let _ = tx.send(result);
            }
        }
    }
}

struct Record {
    segment: u64,
    offset: u64,
    bytes: Vec<u8>,
    index: u64,
    seq: u64,
}

/// The disk work, done in order by the flusher thread: the Raft core never
/// waits on a write, an fsync or an unlink (with the disk busy unpacking
/// image layers they can take seconds, and a Raft core stuck that long
/// misses heartbeats and votes).
enum Job {
    Write { records: Vec<Record>, done: Done },
    Truncate { segment: u64, offset: u64, remove: Vec<PathBuf> },
    Purge { purged: LogId<NodeId>, remove: Vec<PathBuf> },
    Committed(Option<LogId<NodeId>>),
}

/// The log storage handed to openraft.
pub struct LogStore {
    inner: Arc<Mutex<Inner>>,
    flusher: mpsc::Sender<Job>,
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
                // The zero-filled, preallocated rest of the segment.
                if len == 0 && crc == 0 {
                    break;
                }
                let Some(payload) = data.get(pos + HEADER..pos + HEADER + len) else { break };
                if crc32(payload) != crc {
                    break;
                }
                let Ok(entry) = serde_json::from_slice::<Entry<TypeConfig>>(payload) else { break };
                index.insert(entry.log_id.index, Loc { segment: first, offset: pos as u64, len: len as u32, log_id: entry.log_id });
                pos += HEADER + len;
            }
            let rest_is_zero = data[pos..].iter().all(|b| *b == 0);
            if !rest_is_zero {
                eprintln!("apiserver: raft log: cutting {} at byte {pos} of {} (torn or corrupt record)", path.display(), data.len());
                zero_tail(&file, pos as u64, data.len() as u64)?;
                file.sync_all()?;
                broken = true;
            }
            segments.insert(first, Segment { path, file: Some(Arc::new(file)), len: pos as u64 });
        }
        if let Some(purged) = last_purged {
            index.retain(|i, _| *i > purged.index);
        }
        let inner = Arc::new(Mutex::new(Inner {
            dir: dir.to_path_buf(),
            segments,
            index,
            pending: BTreeMap::new(),
            seq: 0,
            last_purged,
            vote,
            committed,
            committed_saved: None,
        }));
        let (tx, rx) = mpsc::channel::<Job>();
        let shared = inner.clone();
        std::thread::Builder::new().name("raft-log-flush".into()).spawn(move || flusher(rx, shared))?;
        Ok(Self { inner, flusher: tx })
    }

    /// Lay out `entries` in the log (in memory) and queue their write.
    fn queue_entries(&self, entries: impl IntoIterator<Item = Entry<TypeConfig>>, done: Done) -> Result<(), StorageError<NodeId>> {
        let mut records = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for entry in entries {
                let payload = serde_json::to_vec(&entry).map_err(log_err)?;
                let first = inner.active(entry.log_id.index, (HEADER + payload.len()) as u64);
                let seg = inner.segments.get_mut(&first).unwrap();
                let mut bytes = Vec::with_capacity(HEADER + payload.len());
                bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
                bytes.extend_from_slice(&payload);
                let loc = Loc { segment: first, offset: seg.len, len: payload.len() as u32, log_id: entry.log_id };
                seg.len += bytes.len() as u64;
                inner.seq += 1;
                let seq = inner.seq;
                inner.index.insert(entry.log_id.index, loc);
                inner.pending.insert(entry.log_id.index, Pending { seq, payload: payload.into() });
                records.push(Record { segment: first, offset: loc.offset, bytes, index: entry.log_id.index, seq });
            }
        }
        self.send(Job::Write { records, done })
    }

    fn send(&self, job: Job) -> Result<(), StorageError<NodeId>> {
        self.flusher.send(job).map_err(|_| log_err(io::Error::other("raft log flusher stopped")))
    }
}

/// Overwrite `[from, to)` with zeros (and make the file at least `to`
/// long): a cut-off tail reads as the end of the log, and the blocks stay
/// allocated, so later appends are overwrites.
fn zero_tail(file: &File, from: u64, to: u64) -> io::Result<()> {
    let zeros = vec![0u8; 1 << 20];
    let mut at = from;
    while at < to {
        let n = (to - at).min(zeros.len() as u64) as usize;
        file.write_all_at(&zeros[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

/// A new segment, written full of zeros up front (as etcd preallocates its
/// WAL): appends then overwrite allocated blocks, so their `fdatasync` has
/// no file size or extent change to journal. On ext4 a journal commit waits
/// for every other dirty file of the disk (image layers being unpacked), and
/// followers could not acknowledge anything for tens of seconds.
fn create_segment(path: &Path) -> io::Result<File> {
    let tmp = path.with_extension("seg.tmp");
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&tmp)?;
    zero_tail(&file, 0, SEGMENT_BYTES)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(file)
}

/// The file of `segment`, opened (created) on first use.
fn segment_file(inner: &Arc<Mutex<Inner>>, segment: u64) -> io::Result<Option<Arc<File>>> {
    let (path, existing) = {
        let guard = inner.lock().unwrap();
        match guard.segments.get(&segment) {
            Some(seg) => (seg.path.clone(), seg.file.clone()),
            // Truncated or purged away since: nothing to write.
            None => return Ok(None),
        }
    };
    if let Some(file) = existing {
        return Ok(Some(file));
    }
    let file = Arc::new(create_segment(&path)?);
    let mut guard = inner.lock().unwrap();
    match guard.segments.get_mut(&segment) {
        Some(seg) => {
            seg.file = Some(file.clone());
            Ok(Some(file))
        }
        None => Ok(None),
    }
}

/// Do the disk work in order, syncing every write that queued up together
/// with one `fdatasync` per file (group commit) before acknowledging them.
fn flusher(rx: mpsc::Receiver<Job>, inner: Arc<Mutex<Inner>>) {
    while let Ok(first) = rx.recv() {
        let mut jobs = vec![first];
        while let Ok(more) = rx.try_recv() {
            jobs.push(more);
        }
        let mut touched: Vec<Arc<File>> = Vec::new();
        let mut written: Vec<(u64, u64)> = Vec::new();
        let mut waiting: Vec<Done> = Vec::new();
        let mut result: io::Result<()> = Ok(());
        for job in jobs {
            match job {
                Job::Write { records, done } => {
                    if result.is_ok() {
                        for r in &records {
                            let write = segment_file(&inner, r.segment).and_then(|f| match f {
                                Some(f) => {
                                    f.write_all_at(&r.bytes, r.offset)?;
                                    if !touched.iter().any(|t| Arc::ptr_eq(t, &f)) {
                                        touched.push(f);
                                    }
                                    Ok(())
                                }
                                None => Ok(()),
                            });
                            if let Err(e) = write {
                                result = Err(e);
                                break;
                            }
                            written.push((r.index, r.seq));
                        }
                    }
                    waiting.push(done);
                }
                Job::Truncate { segment, offset, remove } => {
                    if let Ok(Some(f)) = segment_file(&inner, segment) {
                        let end = f.metadata().map(|m| m.len()).unwrap_or(offset).max(SEGMENT_BYTES);
                        if let Err(e) = zero_tail(&f, offset, end).and_then(|_| f.sync_all()) {
                            result = Err(e);
                        }
                    }
                    for path in remove {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Job::Purge { purged, remove } => {
                    let dir = inner.lock().unwrap().dir.clone();
                    if let Err(e) = write_json_atomic(&dir.join("purged.json"), &purged, true) {
                        eprintln!("apiserver: raft log: saving the purge point: {e}");
                    } else {
                        for path in remove {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
                Job::Committed(committed) => {
                    let dir = inner.lock().unwrap().dir.clone();
                    let _ = write_json_atomic(&dir.join("committed.json"), &committed, false);
                }
            }
        }
        if result.is_ok() {
            for f in &touched {
                if let Err(e) = f.sync_data() {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            let mut guard = inner.lock().unwrap();
            for (index, seq) in written {
                if guard.pending.get(&index).is_some_and(|p| p.seq == seq) {
                    guard.pending.remove(&index);
                }
            }
        }
        for done in waiting {
            let r = match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            done.complete(r);
        }
    }
}

/// Where to read an entry: still in memory, or in its segment file.
enum Source {
    Memory(Arc<[u8]>, LogId<NodeId>),
    File(Option<Arc<File>>, Loc),
}

impl Inner {
    fn read_range(&self, start: Bound<u64>, end: Bound<u64>) -> Vec<Source> {
        self.index
            .range((start, end))
            .map(|(i, loc)| match self.pending.get(i) {
                Some(p) => Source::Memory(p.payload.clone(), loc.log_id),
                None => Source::File(self.segments.get(&loc.segment).and_then(|s| s.file.clone()), *loc),
            })
            .collect()
    }

    /// The segment to append to, a new one (opened by the flusher) when
    /// the last is full.
    fn active(&mut self, next_index: u64, record_len: u64) -> u64 {
        let rotate = match self.segments.iter().next_back() {
            None => true,
            // A record that does not fit the preallocated space goes to a
            // new segment (unless it is bigger than a whole one).
            Some((_, seg)) => seg.len > 0 && seg.len + record_len > SEGMENT_BYTES,
        };
        if rotate {
            let path = segment_path(&self.dir, next_index);
            self.segments.insert(next_index, Segment { path, file: None, len: 0 });
        }
        *self.segments.keys().next_back().unwrap()
    }
}

fn read_entries(sources: Vec<Source>) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        let (bytes, log_id): (std::borrow::Cow<'_, [u8]>, _) = match source {
            Source::Memory(payload, log_id) => (std::borrow::Cow::Owned(payload.to_vec()), log_id),
            Source::File(file, loc) => {
                let file = file.ok_or_else(|| io_err(ErrorSubject::Log(loc.log_id), ErrorVerb::Read, io::Error::other("segment not written")))?;
                let mut buf = vec![0u8; loc.len as usize];
                file.read_exact_at(&mut buf, loc.offset + HEADER as u64)
                    .map_err(|e| io_err(ErrorSubject::Log(loc.log_id), ErrorVerb::Read, e))?;
                (std::borrow::Cow::Owned(buf), loc.log_id)
            }
        };
        let entry = serde_json::from_slice(&bytes).map_err(|e| io_err(ErrorSubject::Log(log_id), ErrorVerb::Read, e))?;
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
        let sources = self.inner.lock().unwrap().read_range(s, e);
        read_entries(sources)
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(&mut self, range: RB) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let (s, e) = bounds(&range);
        let sources = self.inner.lock().unwrap().read_range(s, e);
        read_entries(sources)
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
        // Durable before Raft answers, but off the Raft threads.
        let path = self.inner.lock().unwrap().dir.join("vote.json");
        let v = *vote;
        tokio::task::spawn_blocking(move || write_json_atomic(&path, &v, true))
            .await
            .map_err(|e| io_err(ErrorSubject::Vote, ErrorVerb::Write, io::Error::other(e.to_string())))?
            .map_err(|e| io_err(ErrorSubject::Vote, ErrorVerb::Write, e))?;
        self.inner.lock().unwrap().vote = Some(v);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().vote)
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), StorageError<NodeId>> {
        let queue = {
            let mut inner = self.inner.lock().unwrap();
            inner.committed = committed;
            // Best effort, once a second: on restart it only saves waiting for
            // the leader (a stale value is a smaller one, which is safe).
            let due = inner.committed_saved.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1));
            if due {
                inner.committed_saved = Some(std::time::Instant::now());
            }
            due
        };
        if queue {
            self.send(Job::Committed(committed))?;
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
        self.queue_entries(entries, Done::Raft(callback))
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let job = {
            let mut inner = self.inner.lock().unwrap();
            let Some(first_loc) = inner.index.range(log_id.index..).next().map(|(_, l)| *l) else { return Ok(()) };
            inner.index.retain(|i, _| *i < log_id.index);
            inner.pending.retain(|i, _| *i < log_id.index);
            let doomed: Vec<u64> = inner.segments.range(first_loc.segment + 1..).map(|(k, _)| *k).collect();
            let remove = doomed.into_iter().filter_map(|first| inner.segments.remove(&first)).map(|seg| seg.path).collect();
            inner.segments.get_mut(&first_loc.segment).unwrap().len = first_loc.offset;
            Job::Truncate { segment: first_loc.segment, offset: first_loc.offset, remove }
        };
        self.send(job)
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let job = {
            let mut inner = self.inner.lock().unwrap();
            inner.last_purged = Some(log_id);
            inner.index.retain(|i, _| *i > log_id.index);
            inner.pending.retain(|i, _| *i > log_id.index);
            // A segment goes once the next one starts at or before the first
            // entry still needed; the active (last) one always stays.
            let firsts: Vec<u64> = inner.segments.keys().copied().collect();
            let mut remove = Vec::new();
            for pair in firsts.windows(2) {
                if pair[1] <= log_id.index + 1 {
                    if let Some(seg) = inner.segments.remove(&pair[0]) {
                        remove.push(seg.path);
                    }
                }
            }
            Job::Purge { purged: log_id, remove }
        };
        self.send(job)
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
        let (tx, rx) = mpsc::channel();
        log.queue_entries(entries, Done::Test(tx)).unwrap();
        rx.recv().unwrap().unwrap();
    }

    /// Wait until the flusher has done everything queued so far.
    fn settle(log: &LogStore) {
        let (tx, rx) = mpsc::channel();
        log.send(Job::Write { records: Vec::new(), done: Done::Test(tx) }).unwrap();
        rx.recv().unwrap().unwrap();
    }

    /// Where the records of a segment end (the rest is zeros).
    fn log_end(path: &Path) -> u64 {
        let data = std::fs::read(path).unwrap();
        let mut pos = 0;
        loop {
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            if len == 0 {
                return pos as u64;
            }
            pos += HEADER + len;
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
            settle(&log);
            let got = log.try_get_log_entries(5..=9).await.unwrap();
            assert_eq!(got.iter().map(|e| (e.log_id.leader_id.term, e.log_id.index)).collect::<Vec<_>>(), vec![(1, 5), (1, 6), (1, 7), (2, 8), (2, 9)]);
        }
        let mut log = LogStore::open(dir.path()).unwrap();
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.map(|l| (l.leader_id.term, l.index)), Some((2, 12)));
        assert_eq!(log.read_vote().await.unwrap(), Some(Vote::new(2, 1)));
        log.purge(LogId::new(CommittedLeaderId::new(1, 1), 6)).await.unwrap();
        assert_eq!(log.try_get_log_entries(..).await.unwrap().first().map(|e| e.log_id.index), Some(7));
        settle(&log);
        drop(log);
        let mut log = LogStore::open(dir.path()).unwrap();
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.map(|l| l.index), Some(6));
        assert_eq!(log.try_get_log_entries(..).await.unwrap().len(), 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_entries_read_from_memory() {
        let dir = tempfile::tempdir().unwrap();
        let log = LogStore::open(dir.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        log.queue_entries((1..=5).map(|i| entry(1, i)), Done::Test(tx)).unwrap();
        // Before or after the flusher got to them, they read the same.
        let mut reader = LogReader { inner: log.inner.clone() };
        assert_eq!(reader.try_get_log_entries(..).await.unwrap().len(), 5);
        rx.recv().unwrap().unwrap();
        assert!(log.inner.lock().unwrap().pending.is_empty());
        assert_eq!(reader.try_get_log_entries(2..4).await.unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn torn_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut log = LogStore::open(dir.path()).unwrap();
            append(&mut log, (1..=3).map(|i| entry(1, i)).collect()).await;
        }
        // Tear the third record: garbage where its last bytes were.
        let seg = segment_path(dir.path(), 1);
        let end = log_end(&seg);
        let f = OpenOptions::new().write(true).open(&seg).unwrap();
        f.write_all_at(&[0xff; 3], end - 3).unwrap();
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
        append(&mut log, vec![entry(1, 1)]).await;
        // Pretend the first segment is full so the next append rotates.
        log.inner.lock().unwrap().segments.get_mut(&1).unwrap().len = SEGMENT_BYTES - 10;
        append(&mut log, vec![entry(1, 2), entry(1, 3)]).await;
        assert_eq!(log.inner.lock().unwrap().segments.len(), 2);
        log.purge(LogId::new(CommittedLeaderId::new(1, 1), 1)).await.unwrap();
        settle(&log);
        assert_eq!(log.inner.lock().unwrap().segments.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(!segment_path(dir.path(), 1).exists());
    }
}
