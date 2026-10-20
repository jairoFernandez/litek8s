//! An embedded time-series store for litek8s's own metrics.
//!
//! - **Series** are label sets (with `__name__`), interned and indexed by
//!   label pair in memory; their definitions are appended to `series.log`.
//! - **Samples** go to a write-ahead log first (`wal.log`), then to an
//!   in-memory head chunk per series ([`gorilla`] compressed). Heads are cut
//!   into the current window's open block every `checkpoint` (20 minutes),
//!   or when full, after which the WAL is truncated: a crash loses nothing
//!   that reached the WAL and replays at most one checkpoint of samples.
//! - **Tiers**: raw samples (default 10 s) are kept for a short time; as
//!   each window's block is sealed it is downsampled into the next tier
//!   (default 1 min for 7 days, then 10 min for 90 days): counters keep the
//!   last value of each bucket, gauges the average. Queries read the finest
//!   tier that still covers their start, coarser ones when the step allows,
//!   and fill the part a coarse tier has not received yet from the finer
//!   one on the fly.
//! - **Memory** stays bounded: only the open head chunk of each series (at
//!   most one checkpoint of samples) and the label index live in memory;
//!   blocks are read from disk per query. New series beyond `max_series`
//!   are refused (and counted), so a label explosion cannot grow the
//!   process.
//! - **Staleness**: when a source's batch no longer contains a series it
//!   sent before (a deleted pod), a stale marker ends it, as in Prometheus.
//!
//! On-disk layout under `dir`: `series.log`, `wal.log`, `state.json`, and one
//! directory per tier (`raw`, `60s`, `600s`, ...) of [`block`] files.

pub mod block;
pub mod gorilla;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use block::{OpenBlock, SealedBlock};
use gorilla::Encoder;

/// A series' labels, sorted by name, `__name__` included.
pub type Labels = Vec<(String, String)>;

/// Prometheus's staleness marker: a NaN no computation produces.
pub const STALE_NAN: u64 = 0x7ff0_0000_0000_0002;

pub fn is_stale(v: f64) -> bool {
    v.to_bits() == STALE_NAN
}

pub fn stale() -> f64 {
    f64::from_bits(STALE_NAN)
}

/// Most samples a head chunk takes before it is cut.
const HEAD_MAX: u16 = 120;
/// A window's open block is sealed this long after the window ends (late
/// samples from slow nodes still land in it).
const SEAL_GRACE_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq)]
pub struct TierConfig {
    pub resolution: Duration,
    pub retention: Duration,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// `None`: a temporary directory, removed when the store is dropped.
    pub dir: Option<PathBuf>,
    /// Finest first; the first is the raw sample tier.
    pub tiers: Vec<TierConfig>,
    /// Live series beyond this are refused.
    pub max_series: usize,
    /// How often heads are cut and the WAL truncated.
    pub checkpoint: Duration,
    /// Most samples one query may load.
    pub max_query_samples: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dir: None,
            tiers: default_tiers(),
            max_series: 30_000,
            checkpoint: Duration::from_secs(20 * 60),
            max_query_samples: 2_000_000,
        }
    }
}

pub fn default_tiers() -> Vec<TierConfig> {
    vec![
        TierConfig { resolution: Duration::from_secs(10), retention: Duration::from_secs(6 * 3600) },
        TierConfig { resolution: Duration::from_secs(60), retention: Duration::from_secs(7 * 86400) },
        TierConfig { resolution: Duration::from_secs(600), retention: Duration::from_secs(90 * 86400) },
    ]
}

/// `10s:6h,1m:7d,10m:90d`
pub fn parse_tiers(spec: &str) -> Result<Vec<TierConfig>> {
    let mut tiers = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (res, ret) = part.split_once(':').with_context(|| format!("tier {part:?}: expected <resolution>:<retention>, e.g. 10s:6h"))?;
        let resolution = parse_duration(res)?;
        let retention = parse_duration(ret)?;
        if resolution.as_millis() < 1000 || retention < resolution * 2 {
            bail!("tier {part:?}: resolution must be at least 1s and retention at least twice the resolution");
        }
        if let Some(prev) = tiers.last() {
            let prev: &TierConfig = prev;
            if resolution <= prev.resolution || resolution.as_millis() % prev.resolution.as_millis() != 0 {
                bail!("tier {part:?}: each resolution must be a multiple of the previous one");
            }
        }
        tiers.push(TierConfig { resolution, retention });
    }
    if tiers.is_empty() {
        bail!("no tiers in {spec:?}");
    }
    Ok(tiers)
}

/// `10s`, `5m`, `6h`, `7d`, `2w`, `1y`, `1h30m`, `500ms`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let mut total = 0u128;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        if c == 'm' && chars.peek() == Some(&'s') {
            unit.push(chars.next().unwrap());
        }
        let n: u128 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
        num.clear();
        let ms = match unit.as_str() {
            "ms" => 1,
            "s" => 1000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "w" => 7 * 86_400_000,
            "y" => 365 * 86_400_000,
            _ => bail!("bad duration unit in {s:?}"),
        };
        total += n * ms;
        any = true;
    }
    if !num.is_empty() {
        // A bare number is seconds.
        total += num.parse::<u128>()? * 1000;
        any = true;
    }
    if !any {
        bail!("empty duration");
    }
    Ok(Duration::from_millis(total as u64))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchOp {
    Eq,
    Ne,
    Re,
    Nre,
}

#[derive(Clone, Debug)]
pub struct Matcher {
    pub name: String,
    pub op: MatchOp,
    pub value: String,
    regex: Option<regex::Regex>,
}

impl PartialEq for Matcher {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name && self.op == o.op && self.value == o.value
    }
}

impl Matcher {
    pub fn new(name: &str, op: MatchOp, value: &str) -> Result<Self> {
        let regex = match op {
            MatchOp::Re | MatchOp::Nre => Some(regex::Regex::new(&format!("^(?:{value})$")).with_context(|| format!("bad regex {value:?}"))?),
            _ => None,
        };
        Ok(Matcher { name: name.to_string(), op, value: value.to_string(), regex })
    }

    pub fn eq(name: &str, value: &str) -> Self {
        Matcher { name: name.into(), op: MatchOp::Eq, value: value.into(), regex: None }
    }

    /// Whether a label value (`""` when absent) matches.
    pub fn matches(&self, value: &str) -> bool {
        match self.op {
            MatchOp::Eq => value == self.value,
            MatchOp::Ne => value != self.value,
            MatchOp::Re => self.regex.as_ref().is_some_and(|r| r.is_match(value)),
            MatchOp::Nre => !self.regex.as_ref().is_some_and(|r| r.is_match(value)),
        }
    }
}

/// One series' samples from a query.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesData {
    pub labels: Labels,
    pub samples: Vec<(i64, f64)>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub series: usize,
    pub series_created: u64,
    pub series_refused: u64,
    pub series_removed: u64,
    pub samples_appended: u64,
    pub samples_out_of_order: u64,
    pub head_bytes: usize,
    pub index_bytes: usize,
    pub disk_bytes: u64,
    pub wal_bytes: u64,
    pub blocks: usize,
    pub tiers: Vec<TierStats>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TierStats {
    pub resolution_secs: u64,
    pub retention_secs: u64,
    pub blocks: usize,
    pub bytes: u64,
    pub oldest_ms: Option<i64>,
}

#[derive(Default)]
struct Symbols {
    ids: HashMap<Arc<str>, u32>,
    strs: Vec<Arc<str>>,
}

impl Symbols {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(id) = self.ids.get(s) {
            return *id;
        }
        let a: Arc<str> = Arc::from(s);
        let id = self.strs.len() as u32;
        self.strs.push(a.clone());
        self.ids.insert(a, id);
        id
    }

    fn get(&self, s: &str) -> Option<u32> {
        self.ids.get(s).copied()
    }

    fn str(&self, id: u32) -> &str {
        &self.strs[id as usize]
    }

    fn bytes(&self) -> usize {
        self.strs.iter().map(|s| s.len() + 16 + 8 + 32).sum()
    }
}

struct Series {
    labels: Box<[(u32, u32)]>,
    head: Option<Box<Encoder>>,
    first_t: i64,
    /// Last sample appended (head or blocks).
    last_t: i64,
    /// Last sample already in a block (WAL replay skips up to here).
    flushed_t: i64,
    /// The last sample is a stale marker.
    ended: bool,
    counter: bool,
}

struct Tier {
    res: i64,
    window: i64,
    retention: i64,
    dir: PathBuf,
    open: BTreeMap<i64, OpenBlock>,
    sealed: Vec<SealedBlock>,
    /// Data before this is complete in this tier (all finer windows before
    /// it were downsampled into it). Unused for the raw tier.
    watermark: i64,
}

impl Tier {
    fn window_start(&self, t: i64) -> i64 {
        t.div_euclid(self.window) * self.window
    }
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// resolution (ms) → watermark (ms)
    watermarks: BTreeMap<i64, i64>,
}

struct Inner {
    syms: Symbols,
    series: Vec<Option<Series>>,
    live: usize,
    by_hash: HashMap<u64, u32>,
    collisions: HashMap<u64, Vec<u32>>,
    postings: HashMap<(u32, u32), Vec<u32>>,
    values: HashMap<u32, Vec<u32>>,
    tiers: Vec<Tier>,
    wal: File,
    wal_bytes: u64,
    wal_dirty: bool,
    series_log: File,
    series_log_garbage: usize,
    last_checkpoint: i64,
    sources: HashMap<String, Vec<u32>>,
    metadata: BTreeMap<String, (String, String)>,
    stats: Stats,
}

pub struct Db {
    inner: Mutex<Inner>,
    dir: PathBuf,
    temp: Option<tempfile_dir::TempDir>,
    config: Config,
}

/// A temporary directory (no dev-dependency at runtime).
mod tempfile_dir {
    pub struct TempDir(pub std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn hash_labels(labels: &[(u32, u32)]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    labels.hash(&mut h);
    h.finish()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Whether a series of this name is a counter (downsampled by keeping the
/// last value rather than the average).
pub fn counter_name(name: &str) -> bool {
    name.ends_with("_total") || name.ends_with("_count") || name.ends_with("_sum") || name.ends_with("_bucket")
}

fn valid_label_name(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_') && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Sort, drop empty values and invalid names.
pub fn normalize(mut labels: Labels) -> Labels {
    labels.retain(|(n, v)| !v.is_empty() && valid_label_name(n));
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    labels.dedup_by(|a, b| a.0 == b.0);
    labels
}

fn write_framed(file: &mut File, payload: &[u8]) -> std::io::Result<u64> {
    let mut rec = Vec::with_capacity(payload.len() + 8);
    rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    rec.extend_from_slice(&block::crc(&[payload]).to_le_bytes());
    rec.extend_from_slice(payload);
    file.write_all(&rec)?;
    Ok(rec.len() as u64)
}

/// The framed records of a log file; a torn tail is cut off.
fn read_framed(path: &Path) -> Result<Vec<Vec<u8>>> {
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    }
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if pos + 8 + len > data.len() || block::crc(&[&data[pos + 8..pos + 8 + len]]) != crc {
            break;
        }
        out.push(data[pos + 8..pos + 8 + len].to_vec());
        pos += 8 + len;
    }
    if pos < data.len() {
        eprintln!("obs: {}: cutting off {} bytes of a torn write", path.display(), data.len() - pos);
        OpenOptions::new().write(true).open(path)?.set_len(pos as u64)?;
    }
    Ok(out)
}

fn encode_series(id: u32, labels: &Labels) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&id.to_le_bytes());
    p.extend_from_slice(&(labels.len() as u16).to_le_bytes());
    for (n, v) in labels {
        for s in [n, v] {
            p.extend_from_slice(&(s.len() as u16).to_le_bytes());
            p.extend_from_slice(s.as_bytes());
        }
    }
    p
}

fn decode_series(p: &[u8]) -> Option<(u32, Labels)> {
    let id = u32::from_le_bytes(p.get(0..4)?.try_into().ok()?);
    let n = u16::from_le_bytes(p.get(4..6)?.try_into().ok()?) as usize;
    let mut pos = 6;
    let mut labels = Vec::with_capacity(n);
    let read = |pos: &mut usize| -> Option<String> {
        let l = u16::from_le_bytes(p.get(*pos..*pos + 2)?.try_into().ok()?) as usize;
        let s = String::from_utf8(p.get(*pos + 2..*pos + 2 + l)?.to_vec()).ok()?;
        *pos += 2 + l;
        Some(s)
    };
    for _ in 0..n {
        let name = read(&mut pos)?;
        let value = read(&mut pos)?;
        labels.push((name, value));
    }
    Some((id, labels))
}

/// Average (gauges) or last value (counters) per `res` bucket, stamped at
/// the bucket's start. Stale markers are dropped.
pub fn downsample(samples: &[(i64, f64)], res: i64, counter: bool) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut cur: Option<(i64, f64, u32)> = None;
    for &(t, v) in samples {
        if is_stale(v) {
            continue;
        }
        let b = t.div_euclid(res) * res;
        match &mut cur {
            Some((cb, sum, n)) if *cb == b => {
                if counter {
                    *sum = v;
                } else {
                    *sum += v;
                    *n += 1;
                }
            }
            _ => {
                if let Some((cb, sum, n)) = cur.take() {
                    out.push((cb, if counter { sum } else { sum / n as f64 }));
                }
                cur = Some((b, v, 1));
            }
        }
    }
    if let Some((cb, sum, n)) = cur {
        out.push((cb, if counter { sum } else { sum / n as f64 }));
    }
    out
}

impl Db {
    pub fn open(config: Config) -> Result<Self> {
        let (dir, temp) = match &config.dir {
            Some(d) => (d.clone(), None),
            None => {
                let d = std::env::temp_dir().join(format!("litek8s-obs-{}-{:x}", std::process::id(), now_ms()));
                (d.clone(), Some(tempfile_dir::TempDir(d)))
            }
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let state: State = std::fs::read(dir.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut tiers = Vec::new();
        let mut prev_window = 0i64;
        for (i, tc) in config.tiers.iter().enumerate() {
            let res = tc.resolution.as_millis() as i64;
            let name = if i == 0 { "raw".to_string() } else { format!("{}s", res / 1000) };
            let tdir = dir.join(name);
            std::fs::create_dir_all(&tdir)?;
            let mut window = if i == 0 { res * 720 } else { res * 1440 };
            if prev_window > 0 {
                window = (window + prev_window - 1) / prev_window * prev_window;
            }
            prev_window = window;
            tiers.push(Tier {
                res,
                window,
                retention: tc.retention.as_millis() as i64,
                dir: tdir,
                open: BTreeMap::new(),
                sealed: Vec::new(),
                watermark: state.watermarks.get(&res).copied().unwrap_or(0),
            });
        }
        let series_path = dir.join("series.log");
        let series_records = read_framed(&series_path)?;
        let wal_path = dir.join("wal.log");
        let wal_records = read_framed(&wal_path)?;
        let mut inner = Inner {
            syms: Symbols::default(),
            series: Vec::new(),
            live: 0,
            by_hash: HashMap::new(),
            collisions: HashMap::new(),
            postings: HashMap::new(),
            values: HashMap::new(),
            tiers,
            wal: OpenOptions::new().append(true).create(true).open(&wal_path)?,
            wal_bytes: 0,
            wal_dirty: false,
            series_log: OpenOptions::new().append(true).create(true).open(&series_path)?,
            series_log_garbage: 0,
            last_checkpoint: now_ms(),
            sources: HashMap::new(),
            metadata: BTreeMap::new(),
            stats: Stats::default(),
        };
        for rec in &series_records {
            if let Some((id, labels)) = decode_series(rec) {
                inner.insert_series(id, &labels);
            }
        }
        // Blocks: sealed ones, open ones (a stale open file next to its
        // sealed block is the leftover of an interrupted seal).
        for tier in &mut inner.tiers {
            let mut opens = Vec::new();
            for entry in std::fs::read_dir(&tier.dir)?.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(b) = SealedBlock::parse(&path) {
                    tier.sealed.push(b);
                } else if let Some(start) = name.strip_suffix(".open").and_then(|s| s.parse::<i64>().ok()) {
                    opens.push(start);
                } else if name.ends_with(".tmp") {
                    let _ = std::fs::remove_file(&path);
                }
            }
            tier.sealed.sort_by_key(|b| b.start);
            for start in opens {
                let end = start + tier.window;
                if tier.sealed.iter().any(|b| b.start == start) {
                    let _ = std::fs::remove_file(OpenBlock::path_for(&tier.dir, start));
                    continue;
                }
                tier.open.insert(start, OpenBlock::open(&tier.dir, start, end)?);
            }
        }
        // Times of each series from block indexes.
        let mut times: Vec<(u32, i64, i64, bool)> = Vec::new();
        for (ti, tier) in inner.tiers.iter().enumerate() {
            for b in tier.open.values() {
                times.extend(b.chunks.iter().map(|c| (c.id, c.mint, c.maxt, ti == 0)));
            }
            for b in &tier.sealed {
                match b.index() {
                    Ok(index) => times.extend(index.into_iter().map(|(id, mint, maxt)| (id, mint, maxt, ti == 0))),
                    Err(e) => eprintln!("obs: {e:#}"),
                }
            }
        }
        for (id, mint, maxt, raw) in times {
            if let Some(Some(s)) = inner.series.get_mut(id as usize) {
                s.first_t = if s.first_t == 0 { mint } else { s.first_t.min(mint) };
                s.last_t = s.last_t.max(maxt);
                if raw {
                    s.flushed_t = s.flushed_t.max(maxt);
                }
            }
        }
        // Replay the WAL into heads.
        let mut replayed = 0usize;
        for rec in &wal_records {
            if rec.len() < 12 {
                continue;
            }
            let ts = i64::from_le_bytes(rec[0..8].try_into().unwrap());
            let n = u32::from_le_bytes(rec[8..12].try_into().unwrap()) as usize;
            for i in 0..n {
                let at = 12 + i * 12;
                let Some(s) = rec.get(at..at + 12) else { break };
                let id = u32::from_le_bytes(s[0..4].try_into().unwrap());
                let v = f64::from_bits(u64::from_le_bytes(s[4..12].try_into().unwrap()));
                let flushed = inner.series.get(id as usize).and_then(|s| s.as_ref()).map(|s| s.flushed_t);
                if flushed.is_some_and(|f| ts > f) {
                    inner.push(id, ts, v)?;
                    replayed += 1;
                }
            }
        }
        inner.wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        if replayed > 0 {
            eprintln!("obs: replayed {replayed} samples from the WAL");
        }
        Ok(Db { inner: Mutex::new(inner), dir, temp, config })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn is_temporary(&self) -> bool {
        self.temp.is_some()
    }

    /// Record a metric's type and help (for `/api/v1/metadata`).
    pub fn set_metadata(&self, name: &str, kind: &str, help: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.metadata.get(name).is_none_or(|(k, h)| k != kind || h != help) {
            inner.metadata.insert(name.to_string(), (kind.to_string(), help.to_string()));
        }
    }

    pub fn metadata(&self) -> BTreeMap<String, (String, String)> {
        self.inner.lock().unwrap().metadata.clone()
    }

    /// The id of a series, created if new (`None`: refused, over budget).
    pub fn series_id(&self, labels: Labels) -> Option<u32> {
        let labels = normalize(labels);
        let mut inner = self.inner.lock().unwrap();
        inner.get_or_create(&labels, self.config.max_series)
    }

    /// Append one batch of a source (a node, the server's own collectors):
    /// series it sent last time and not now get a stale marker.
    pub fn append_ids(&self, source: &str, ts: i64, samples: &[(u32, f64)]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let mut batch: Vec<(u32, f64)> = samples.to_vec();
        let mut ids: Vec<u32> = samples.iter().map(|s| s.0).collect();
        ids.sort_unstable();
        ids.dedup();
        if let Some(prev) = inner.sources.get(source) {
            for id in prev {
                if ids.binary_search(id).is_err() {
                    let gone = inner.series.get(*id as usize).and_then(|s| s.as_ref()).is_some_and(|s| !s.ended && s.last_t > 0);
                    if gone {
                        batch.push((*id, stale()));
                    }
                }
            }
        }
        inner.sources.insert(source.to_string(), ids);
        inner.write_wal(ts, &batch)?;
        for (id, v) in batch {
            inner.push(id, ts, v)?;
        }
        Ok(())
    }

    /// [`Db::append_ids`] with label sets.
    pub fn append(&self, source: &str, ts: i64, samples: Vec<(Labels, f64)>) -> Result<()> {
        let ids: Vec<(u32, f64)> = {
            let mut inner = self.inner.lock().unwrap();
            samples.into_iter().filter_map(|(l, v)| Some((inner.get_or_create(&normalize(l), self.config.max_series)?, v))).collect()
        };
        self.append_ids(source, ts, &ids)
    }

    /// Forget a source's series set (it went away): its series get stale
    /// markers now.
    pub fn end_source(&self, source: &str, ts: i64) -> Result<()> {
        if self.inner.lock().unwrap().sources.contains_key(source) {
            self.append_ids(source, ts, &[])?;
            self.inner.lock().unwrap().sources.remove(source);
        }
        Ok(())
    }

    /// Periodic work: fsync the WAL, cut heads at checkpoints, seal finished
    /// windows, downsample, apply retention, drop dead series.
    pub fn maintain(&self, now: i64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.wal_dirty {
            inner.wal.sync_data()?;
            inner.wal_dirty = false;
        }
        let due = now - inner.last_checkpoint >= self.config.checkpoint.as_millis() as i64;
        let raw = &inner.tiers[0];
        let seal_due = raw.open.keys().any(|s| s + raw.window + SEAL_GRACE_MS <= now);
        if due || seal_due {
            inner.checkpoint(now)?;
        }
        inner.seal_and_downsample(now, &self.dir)?;
        inner.retention(now);
        inner.gc_series(now, &self.dir)?;
        Ok(())
    }

    /// Cut every head and truncate the WAL now (before a shutdown).
    pub fn flush(&self) -> Result<()> {
        self.inner.lock().unwrap().checkpoint(now_ms())
    }

    pub fn stats(&self) -> Stats {
        let inner = self.inner.lock().unwrap();
        let mut st = inner.stats.clone();
        st.series = inner.live;
        st.head_bytes = inner.series.iter().flatten().map(|s| s.head.as_ref().map_or(0, |h| h.heap_bytes() + 80)).sum();
        st.index_bytes = inner.index_bytes();
        st.wal_bytes = inner.wal_bytes;
        st.tiers = inner
            .tiers
            .iter()
            .map(|t| TierStats {
                resolution_secs: (t.res / 1000) as u64,
                retention_secs: (t.retention / 1000) as u64,
                blocks: t.sealed.len() + t.open.len(),
                bytes: t.sealed.iter().map(|b| b.size).sum::<u64>() + t.open.values().map(|b| b.size()).sum::<u64>(),
                oldest_ms: t.sealed.first().map(|b| b.start).or_else(|| t.open.keys().next().copied()),
            })
            .collect();
        st.blocks = st.tiers.iter().map(|t| t.blocks).sum();
        st.disk_bytes = st.tiers.iter().map(|t| t.bytes).sum::<u64>() + inner.wal_bytes + inner.series_log.metadata().map(|m| m.len()).unwrap_or(0);
        st
    }

    /// Estimated memory held (index + heads), bytes.
    pub fn memory_bytes(&self) -> usize {
        let st = self.stats();
        st.head_bytes + st.index_bytes
    }

    fn matching_ids(inner: &Inner, matchers: &[Matcher], mint: i64, maxt: i64) -> Vec<u32> {
        // Candidates from the most selective matcher we can use an index for.
        let mut cand: Option<Vec<u32>> = None;
        for m in matchers {
            if m.op == MatchOp::Eq && !m.value.is_empty() {
                let ids = match (inner.syms.get(&m.name), inner.syms.get(&m.value)) {
                    (Some(n), Some(v)) => inner.postings.get(&(n, v)).cloned().unwrap_or_default(),
                    _ => Vec::new(),
                };
                if cand.as_ref().is_none_or(|c| ids.len() < c.len()) {
                    cand = Some(ids);
                }
            }
        }
        if cand.is_none() {
            if let Some(m) = matchers.iter().find(|m| m.op == MatchOp::Re && !m.matches("")) {
                let mut ids = Vec::new();
                if let Some(n) = inner.syms.get(&m.name) {
                    for v in inner.values.get(&n).into_iter().flatten() {
                        if m.matches(inner.syms.str(*v)) {
                            ids.extend(inner.postings.get(&(n, *v)).into_iter().flatten());
                        }
                    }
                }
                ids.sort_unstable();
                cand = Some(ids);
            }
        }
        let cand = cand.unwrap_or_else(|| (0..inner.series.len() as u32).collect());
        cand.into_iter()
            .filter(|id| {
                let Some(Some(s)) = inner.series.get(*id as usize) else { return false };
                if s.last_t < mint || (s.first_t > maxt && s.first_t != 0) {
                    return false;
                }
                matchers.iter().all(|m| {
                    let v = inner.syms.get(&m.name).and_then(|n| s.labels.iter().find(|(ln, _)| *ln == n)).map(|(_, v)| inner.syms.str(*v)).unwrap_or("");
                    m.matches(v)
                })
            })
            .collect()
    }

    fn labels_of(inner: &Inner, id: u32) -> Labels {
        inner.series[id as usize]
            .as_ref()
            .map(|s| s.labels.iter().map(|(n, v)| (inner.syms.str(*n).to_string(), inner.syms.str(*v).to_string())).collect())
            .unwrap_or_default()
    }

    /// Label sets of the series matching any of `selectors` with data in
    /// [mint, maxt].
    pub fn series(&self, selectors: &[Vec<Matcher>], mint: i64, maxt: i64) -> Vec<Labels> {
        let inner = self.inner.lock().unwrap();
        let mut ids: Vec<u32> = selectors.iter().flat_map(|m| Self::matching_ids(&inner, m, mint, maxt)).collect();
        if selectors.is_empty() {
            ids = Self::matching_ids(&inner, &[], mint, maxt);
        }
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter().map(|id| Self::labels_of(&inner, id)).collect()
    }

    pub fn label_names(&self, selectors: &[Vec<Matcher>], mint: i64, maxt: i64) -> Vec<String> {
        let mut names: Vec<String> = self.series(selectors, mint, maxt).into_iter().flat_map(|l| l.into_iter().map(|(n, _)| n)).collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn label_values(&self, name: &str, selectors: &[Vec<Matcher>], mint: i64, maxt: i64) -> Vec<String> {
        let mut values: Vec<String> =
            self.series(selectors, mint, maxt).into_iter().filter_map(|l| l.into_iter().find(|(n, _)| n == name).map(|(_, v)| v)).collect();
        values.sort();
        values.dedup();
        values
    }

    /// The resolution (ms) a select with these arguments reads at.
    pub fn resolution_for(&self, mint: i64, max_res: i64) -> i64 {
        let inner = self.inner.lock().unwrap();
        inner.tiers[inner.pick_tier(mint, max_res, now_ms())].res
    }

    /// Samples of the series matching `matchers` in [mint, maxt], from the
    /// coarsest tier whose resolution is at most `max_res` (ms) and that
    /// still covers `mint`.
    pub fn select(&self, matchers: &[Matcher], mint: i64, maxt: i64, max_res: i64) -> Result<Vec<SeriesData>> {
        let now = now_ms();
        let (plan, ids, labels, counters) = {
            let inner = self.inner.lock().unwrap();
            let ids = Self::matching_ids(&inner, matchers, mint, maxt);
            let tier = inner.pick_tier(mint, max_res, now);
            let set: HashSet<u32> = ids.iter().copied().collect();
            let plan = inner.plan(tier, &set, mint, maxt);
            let labels: Vec<Labels> = ids.iter().map(|id| Self::labels_of(&inner, *id)).collect();
            let counters: Vec<bool> = ids.iter().map(|id| inner.series[*id as usize].as_ref().is_some_and(|s| s.counter)).collect();
            (plan, ids, labels, counters)
        };
        let mut budget = self.config.max_query_samples;
        let mut out = Vec::with_capacity(ids.len());
        let index: HashMap<u32, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut per: Vec<Vec<(i64, f64)>> = vec![Vec::new(); ids.len()];
        plan.read(&index, &counters, &mut per, &mut budget)?;
        for ((labels, mut samples), _) in labels.into_iter().zip(per).zip(&ids) {
            samples.sort_by_key(|s| s.0);
            samples.dedup_by_key(|s| s.0);
            if !samples.is_empty() {
                out.push(SeriesData { labels, samples });
            }
        }
        Ok(out)
    }
}

/// What a select reads, gathered under the lock and read without it.
struct Plan {
    /// (res of the tier read, downsample to this res (0 = as is), parts)
    parts: Vec<PlanPart>,
}

struct PlanPart {
    /// Downsample what this part reads to this resolution (0: no).
    downsample: i64,
    mint: i64,
    maxt: i64,
    sealed: Vec<SealedBlock>,
    open: Vec<(PathBuf, Vec<block::ChunkRef>)>,
    heads: Vec<(u32, Vec<u8>)>,
    ids: HashSet<u32>,
}

impl Plan {
    fn read(&self, index: &HashMap<u32, usize>, counters: &[bool], per: &mut [Vec<(i64, f64)>], budget: &mut usize) -> Result<()> {
        for part in &self.parts {
            let mut got: Vec<Vec<(i64, f64)>> = vec![Vec::new(); per.len()];
            let mut take = |id: u32, chunk: &[u8], got: &mut Vec<Vec<(i64, f64)>>| -> Result<()> {
                if let Some(i) = index.get(&id) {
                    let before = got[*i].len();
                    gorilla::decode_range(chunk, part.mint, part.maxt, &mut got[*i]);
                    let n = got[*i].len() - before;
                    if n > *budget {
                        bail!("query would load too many samples (limit {}): narrow the selector or the time range", n + *budget);
                    }
                    *budget -= n;
                }
                Ok(())
            };
            for b in &part.sealed {
                let Ok((file, refs)) = b.refs(&part.ids, part.mint, part.maxt) else { continue };
                for r in refs {
                    if let Ok(chunk) = block::read_chunk(&file, &r) {
                        take(r.id, &chunk, &mut got)?;
                    }
                }
            }
            for (path, refs) in &part.open {
                let Ok(file) = File::open(path) else { continue };
                for r in refs {
                    if let Ok(chunk) = block::read_chunk(&file, r) {
                        take(r.id, &chunk, &mut got)?;
                    }
                }
            }
            for (id, chunk) in &part.heads {
                take(*id, chunk, &mut got)?;
            }
            for (i, mut samples) in got.into_iter().enumerate() {
                if part.downsample > 0 {
                    samples.sort_by_key(|s| s.0);
                    samples.dedup_by_key(|s| s.0);
                    samples = downsample(&samples, part.downsample, counters[i]);
                }
                per[i].extend(samples);
            }
        }
        Ok(())
    }
}

impl Inner {
    fn insert_series(&mut self, id: u32, labels: &Labels) {
        let pairs: Box<[(u32, u32)]> = labels.iter().map(|(n, v)| (self.syms.intern(n), self.syms.intern(v))).collect();
        let h = hash_labels(&pairs);
        let name = labels.iter().find(|(n, _)| n == "__name__").map(|(_, v)| v.as_str()).unwrap_or("");
        let counter = self.metadata.get(name).map(|(k, _)| k == "counter" || k == "histogram" || k == "summary").unwrap_or_else(|| counter_name(name));
        if self.series.len() <= id as usize {
            self.series.resize_with(id as usize + 1, || None);
        }
        if self.series[id as usize].is_some() {
            return;
        }
        for (n, v) in pairs.iter() {
            let p = self.postings.entry((*n, *v)).or_default();
            if let Err(pos) = p.binary_search(&id) {
                p.insert(pos, id);
            }
            let vals = self.values.entry(*n).or_default();
            if let Err(pos) = vals.binary_search(v) {
                vals.insert(pos, *v);
            }
        }
        match self.by_hash.get(&h) {
            None => {
                self.by_hash.insert(h, id);
            }
            Some(_) => self.collisions.entry(h).or_default().push(id),
        }
        self.series[id as usize] = Some(Series { labels: pairs, head: None, first_t: 0, last_t: 0, flushed_t: 0, ended: false, counter });
        self.live += 1;
    }

    fn find(&self, labels: &Labels) -> Option<u32> {
        let pairs: Vec<(u32, u32)> = labels.iter().map(|(n, v)| Some((self.syms.get(n)?, self.syms.get(v)?))).collect::<Option<_>>()?;
        let h = hash_labels(&pairs);
        let check = |id: u32| self.series.get(id as usize).and_then(|s| s.as_ref()).is_some_and(|s| *s.labels == pairs[..]);
        if let Some(id) = self.by_hash.get(&h).copied().filter(|id| check(*id)) {
            return Some(id);
        }
        self.collisions.get(&h)?.iter().copied().find(|id| check(*id))
    }

    fn get_or_create(&mut self, labels: &Labels, max_series: usize) -> Option<u32> {
        if let Some(id) = self.find(labels) {
            return Some(id);
        }
        if self.live >= max_series || labels.is_empty() {
            self.stats.series_refused += 1;
            return None;
        }
        let id = self.series.len() as u32;
        if let Err(e) = write_framed(&mut self.series_log, &encode_series(id, labels)) {
            eprintln!("obs: cannot record a new series: {e}");
            return None;
        }
        self.insert_series(id, labels);
        self.stats.series_created += 1;
        Some(id)
    }

    fn remove_series(&mut self, id: u32) {
        let Some(s) = self.series.get_mut(id as usize).and_then(Option::take) else { return };
        for (n, v) in s.labels.iter() {
            if let Some(p) = self.postings.get_mut(&(*n, *v)) {
                if let Ok(pos) = p.binary_search(&id) {
                    p.remove(pos);
                }
                if p.is_empty() {
                    self.postings.remove(&(*n, *v));
                    if let Some(vals) = self.values.get_mut(n) {
                        vals.retain(|x| x != v);
                    }
                }
            }
        }
        let h = hash_labels(&s.labels);
        if self.by_hash.get(&h) == Some(&id) {
            self.by_hash.remove(&h);
            if let Some(mut c) = self.collisions.remove(&h) {
                if let Some(next) = c.pop() {
                    self.by_hash.insert(h, next);
                    if !c.is_empty() {
                        self.collisions.insert(h, c);
                    }
                }
            }
        } else if let Some(c) = self.collisions.get_mut(&h) {
            c.retain(|x| *x != id);
        }
        self.live -= 1;
        self.series_log_garbage += 1;
        self.stats.series_removed += 1;
    }

    fn index_bytes(&self) -> usize {
        let series: usize = self.series.iter().map(|s| 8 + s.as_ref().map_or(0, |s| 64 + s.labels.len() * 8)).sum();
        let postings: usize = self.postings.values().map(|p| 48 + p.capacity() * 4).sum();
        let values: usize = self.values.values().map(|v| 40 + v.capacity() * 4).sum();
        series + postings + values + self.by_hash.len() * 24 + self.syms.bytes()
            + self.sources.values().map(|v| 64 + v.capacity() * 4).sum::<usize>()
    }

    fn write_wal(&mut self, ts: i64, batch: &[(u32, f64)]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut p = Vec::with_capacity(12 + batch.len() * 12);
        p.extend_from_slice(&ts.to_le_bytes());
        p.extend_from_slice(&(batch.len() as u32).to_le_bytes());
        for (id, v) in batch {
            p.extend_from_slice(&id.to_le_bytes());
            p.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        self.wal_bytes += write_framed(&mut self.wal, &p)?;
        self.wal_dirty = true;
        Ok(())
    }

    fn push(&mut self, id: u32, ts: i64, v: f64) -> Result<()> {
        let window = self.tiers[0].window;
        let Some(Some(s)) = self.series.get_mut(id as usize) else { return Ok(()) };
        if ts <= s.last_t {
            self.stats.samples_out_of_order += 1;
            return Ok(());
        }
        let cut = match &s.head {
            Some(h) => h.count() >= HEAD_MAX || h.first_t().div_euclid(window) != ts.div_euclid(window),
            None => false,
        };
        if cut {
            self.cut_head(id)?;
        }
        let Some(Some(s)) = self.series.get_mut(id as usize) else { return Ok(()) };
        s.head.get_or_insert_with(Default::default).push(ts, v);
        if s.first_t == 0 {
            s.first_t = ts;
        }
        s.last_t = ts;
        s.ended = is_stale(v);
        self.stats.samples_appended += 1;
        Ok(())
    }

    /// Move a series' head chunk into its window's open block.
    fn cut_head(&mut self, id: u32) -> Result<()> {
        let Some(Some(s)) = self.series.get_mut(id as usize) else { return Ok(()) };
        let Some(head) = s.head.take() else { return Ok(()) };
        if head.count() == 0 {
            return Ok(());
        }
        let (mint, maxt) = (head.first_t(), head.last_t());
        s.flushed_t = s.flushed_t.max(maxt);
        let chunk = head.encode();
        let tier = &mut self.tiers[0];
        let start = tier.window_start(mint);
        if tier.sealed.iter().any(|b| b.start == start) {
            // Too late for a sealed window (a node far behind): dropped.
            self.stats.samples_out_of_order += head.count() as u64;
            return Ok(());
        }
        let block = match tier.open.entry(start) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => e.insert(OpenBlock::open(&tier.dir, start, start + tier.window)?),
        };
        block.append(id, mint, maxt, &chunk)?;
        Ok(())
    }

    fn checkpoint(&mut self, now: i64) -> Result<()> {
        let ids: Vec<u32> = self.series.iter().enumerate().filter(|(_, s)| s.as_ref().is_some_and(|s| s.head.is_some())).map(|(i, _)| i as u32).collect();
        for id in ids {
            self.cut_head(id)?;
        }
        for tier in &mut self.tiers {
            for b in tier.open.values_mut() {
                b.sync()?;
            }
        }
        self.wal.set_len(0)?;
        self.wal.sync_all()?;
        self.wal_bytes = 0;
        self.wal_dirty = false;
        self.last_checkpoint = now;
        Ok(())
    }

    fn save_state(&self, dir: &Path) -> Result<()> {
        let state = State { watermarks: self.tiers.iter().skip(1).map(|t| (t.res, t.watermark)).collect() };
        let tmp = dir.join("state.json.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(&state)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join("state.json"))?;
        block::sync_dir(dir);
        Ok(())
    }

    fn seal_and_downsample(&mut self, now: i64, dir: &Path) -> Result<()> {
        for ti in 0..self.tiers.len() {
            loop {
                let tier = &self.tiers[ti];
                let ready = |start: i64| {
                    let end = start + tier.window;
                    end + SEAL_GRACE_MS <= now && (ti == 0 || tier.watermark >= end)
                };
                let Some(start) = tier.open.keys().copied().find(|s| ready(*s)) else { break };
                let open = self.tiers[ti].open.remove(&start).unwrap();
                let path = open.seal(&self.tiers[ti].dir)?;
                let sealed = SealedBlock::parse(&path).context("sealed block")?;
                self.tiers[ti].sealed.push(sealed.clone());
                self.tiers[ti].sealed.sort_by_key(|b| b.start);
                if ti + 1 < self.tiers.len() {
                    self.downsample_into(ti + 1, &sealed)?;
                    self.save_state(dir)?;
                }
            }
            // A finer window may have been sealed earlier without a
            // downsample (the tier was added later): catch up.
            if ti + 1 < self.tiers.len() {
                let pending: Vec<SealedBlock> = self.tiers[ti].sealed.iter().filter(|b| b.end > self.tiers[ti + 1].watermark).cloned().collect();
                for b in pending {
                    self.downsample_into(ti + 1, &b)?;
                }
                if !self.tiers[ti].sealed.is_empty() {
                    self.save_state(dir)?;
                }
            }
        }
        Ok(())
    }

    fn downsample_into(&mut self, ti: usize, src: &SealedBlock) -> Result<()> {
        let res = self.tiers[ti].res;
        let (file, refs) = src.all()?;
        let mut by_id: BTreeMap<u32, Vec<(i64, f64)>> = BTreeMap::new();
        for r in &refs {
            if let Ok(chunk) = block::read_chunk(&file, r) {
                gorilla::decode(&chunk, by_id.entry(r.id).or_default());
            }
        }
        for (id, mut samples) in by_id {
            let counter = self.series.get(id as usize).and_then(|s| s.as_ref()).is_some_and(|s| s.counter);
            samples.sort_by_key(|s| s.0);
            samples.dedup_by_key(|s| s.0);
            let points = downsample(&samples, res, counter);
            // One chunk per destination window (a source window never
            // spans two: windows are multiples of each other).
            let mut i = 0;
            while i < points.len() {
                let mut enc = Encoder::new();
                let start = self.tiers[ti].window_start(points[i].0);
                while i < points.len() && self.tiers[ti].window_start(points[i].0) == start && enc.count() < u16::MAX {
                    enc.push(points[i].0, points[i].1);
                    i += 1;
                }
                let tier = &mut self.tiers[ti];
                if tier.sealed.iter().any(|b| b.start == start) {
                    continue;
                }
                let block = match tier.open.entry(start) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(e) => e.insert(OpenBlock::open(&tier.dir, start, start + tier.window)?),
                };
                block.append(id, enc.first_t(), enc.last_t(), &enc.encode())?;
            }
        }
        let tier = &mut self.tiers[ti];
        for b in tier.open.values_mut() {
            b.sync()?;
        }
        tier.watermark = tier.watermark.max(src.end);
        Ok(())
    }

    fn retention(&mut self, now: i64) {
        let n = self.tiers.len();
        for ti in 0..n {
            let next_wm = if ti + 1 < n { self.tiers[ti + 1].watermark } else { i64::MAX };
            let tier = &mut self.tiers[ti];
            let cutoff = now - tier.retention;
            tier.sealed.retain(|b| {
                let expired = b.end <= cutoff && b.end <= next_wm;
                if expired {
                    let _ = std::fs::remove_file(&b.path);
                }
                !expired
            });
        }
    }

    /// Drop series with no data left in any tier; rewrite `series.log` when
    /// a quarter of it is garbage.
    fn gc_series(&mut self, now: i64, dir: &Path) -> Result<()> {
        let longest = self.tiers.iter().map(|t| t.retention + t.window).max().unwrap_or(0);
        let dead: Vec<u32> = self
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_some_and(|s| s.head.is_none() && s.last_t > 0 && s.last_t < now - longest))
            .map(|(i, _)| i as u32)
            .collect();
        for id in dead {
            self.remove_series(id);
        }
        if self.series_log_garbage > 1000 && self.series_log_garbage * 4 > self.live {
            let tmp = dir.join("series.log.tmp");
            let mut f = File::create(&tmp)?;
            for id in 0..self.series.len() as u32 {
                if self.series[id as usize].is_some() {
                    let labels = Db::labels_of(self, id);
                    write_framed(&mut f, &encode_series(id, &labels))?;
                }
            }
            f.sync_all()?;
            std::fs::rename(&tmp, dir.join("series.log"))?;
            block::sync_dir(dir);
            self.series_log = OpenOptions::new().append(true).open(dir.join("series.log"))?;
            self.series_log_garbage = 0;
        }
        Ok(())
    }

    fn pick_tier(&self, mint: i64, max_res: i64, now: i64) -> usize {
        let covering: Vec<usize> = (0..self.tiers.len()).filter(|i| now - self.tiers[*i].retention <= mint).collect();
        if covering.is_empty() {
            return self.tiers.len() - 1;
        }
        covering.iter().rev().copied().find(|i| self.tiers[*i].res <= max_res).unwrap_or(covering[0])
    }

    /// Parts to read for tier `ti` over [mint, maxt]: its own blocks up to
    /// its watermark, the finer tier (downsampled on the fly) after it.
    fn plan(&self, ti: usize, ids: &HashSet<u32>, mint: i64, maxt: i64) -> Plan {
        let mut parts = Vec::new();
        self.plan_into(ti, ids, mint, maxt, 0, &mut parts);
        Plan { parts }
    }

    fn plan_into(&self, ti: usize, ids: &HashSet<u32>, mint: i64, maxt: i64, downsample: i64, parts: &mut Vec<PlanPart>) {
        if mint > maxt {
            return;
        }
        let tier = &self.tiers[ti];
        let own_max = if ti == 0 { maxt } else { maxt.min(tier.watermark - 1) };
        if own_max >= mint {
            let sealed = tier.sealed.iter().filter(|b| b.end > mint && b.start <= own_max).cloned().collect();
            let open = tier
                .open
                .values()
                .filter(|b| b.end > mint && b.start <= own_max)
                .map(|b| (b.path.clone(), b.refs(ids, mint, own_max)))
                .filter(|(_, r)| !r.is_empty())
                .collect();
            let heads = if ti == 0 {
                ids.iter()
                    .filter_map(|id| {
                        let h = self.series.get(*id as usize)?.as_ref()?.head.as_ref()?;
                        (h.last_t() >= mint && h.first_t() <= maxt).then(|| (*id, h.encode()))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            parts.push(PlanPart { downsample, mint, maxt: own_max, sealed, open, heads, ids: ids.clone() });
        }
        if ti > 0 && maxt >= tier.watermark {
            let from = mint.max(tier.watermark);
            let ds = if downsample > 0 { downsample } else { tier.res };
            self.plan_into(ti - 1, ids, from, maxt, ds, parts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(name: &str, pod: &str) -> Labels {
        vec![("__name__".into(), name.into()), ("pod".into(), pod.into())]
    }

    fn config(dir: &Path) -> Config {
        Config {
            dir: Some(dir.to_path_buf()),
            tiers: vec![
                TierConfig { resolution: Duration::from_secs(10), retention: Duration::from_secs(4 * 3600) },
                TierConfig { resolution: Duration::from_secs(60), retention: Duration::from_secs(2 * 86400) },
                TierConfig { resolution: Duration::from_secs(600), retention: Duration::from_secs(30 * 86400) },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn parses_tiers_and_durations() {
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_tiers("10s:6h,1m:7d,10m:90d").unwrap(), default_tiers());
        assert!(parse_tiers("10s:6h,15s:7d").is_err());
        assert!(parse_tiers("10s:10s").is_err());
    }

    #[test]
    fn append_select_and_matchers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(config(dir.path())).unwrap();
        let t0 = now_ms() / 10_000 * 10_000 - 600_000;
        for i in 0..60 {
            db.append("n1", t0 + i * 10_000, vec![(labels("cpu_total", "a"), i as f64), (labels("cpu_total", "b"), 2.0 * i as f64), (labels("mem", "a"), 5.0)])
                .unwrap();
        }
        let all = db.select(&[Matcher::eq("__name__", "cpu_total")], t0, t0 + 600_000, 10_000).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].samples.len(), 60);
        let b = db.select(&[Matcher::eq("__name__", "cpu_total"), Matcher::new("pod", MatchOp::Re, "b|c").unwrap()], t0, t0 + 600_000, 10_000).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].samples[3], (t0 + 30_000, 6.0));
        let not_a = db.select(&[Matcher::new("pod", MatchOp::Ne, "a").unwrap()], t0, t0 + 600_000, 10_000).unwrap();
        assert_eq!(not_a.len(), 1);
        assert_eq!(db.label_values("pod", &[], t0, t0 + 600_000), vec!["a", "b"]);
        assert_eq!(db.label_names(&[], t0, t0 + 600_000), vec!["__name__", "pod"]);
        // Out of order is refused.
        db.append("other", t0, vec![(labels("mem", "a"), 1.0)]).unwrap();
        assert_eq!(db.stats().samples_out_of_order, 1);
    }

    #[test]
    fn stale_markers_end_series_of_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(config(dir.path())).unwrap();
        let t0 = now_ms() - 60_000;
        db.append("n1", t0, vec![(labels("up", "a"), 1.0), (labels("up", "b"), 1.0)]).unwrap();
        db.append("n1", t0 + 10_000, vec![(labels("up", "a"), 1.0)]).unwrap();
        let b = db.select(&[Matcher::eq("pod", "b")], t0, t0 + 20_000, 10_000).unwrap();
        assert!(is_stale(b[0].samples[1].1));
        // Another source does not end n1's series.
        db.append("n2", t0 + 20_000, vec![(labels("x", "z"), 1.0)]).unwrap();
        let a = db.select(&[Matcher::eq("pod", "a")], t0, t0 + 30_000, 10_000).unwrap();
        assert_eq!(a[0].samples.len(), 2);
    }

    #[test]
    fn survives_restart_through_wal_and_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = now_ms() / 10_000 * 10_000 - 3_000_000;
        {
            let db = Db::open(config(dir.path())).unwrap();
            for i in 0..300 {
                db.append("n1", t0 + i * 10_000, vec![(labels("c_total", "a"), i as f64)]).unwrap();
            }
            // 300 samples: two heads cut at 120, the rest only in the WAL.
            db.maintain(t0 + 300 * 10_000).unwrap();
        }
        let db = Db::open(config(dir.path())).unwrap();
        let got = db.select(&[Matcher::eq("pod", "a")], t0, t0 + 3_000_000, 10_000).unwrap();
        assert_eq!(got[0].samples.len(), 300);
        assert_eq!(got[0].samples[299], (t0 + 2_990_000, 299.0));
        // And appending continues after the replayed samples.
        db.append("n1", t0 + 300 * 10_000, vec![(labels("c_total", "a"), 300.0)]).unwrap();
        db.flush().unwrap();
        drop(db);
        let db = Db::open(config(dir.path())).unwrap();
        let got = db.select(&[Matcher::eq("pod", "a")], t0, t0 + 3_100_000, 10_000).unwrap();
        assert_eq!(got[0].samples.len(), 301);
    }

    #[test]
    fn torn_wal_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = now_ms() - 100_000;
        {
            let db = Db::open(config(dir.path())).unwrap();
            db.append("n1", t0, vec![(labels("g", "a"), 1.0)]).unwrap();
            db.append("n1", t0 + 10_000, vec![(labels("g", "a"), 2.0)]).unwrap();
        }
        let wal = dir.path().join("wal.log");
        let len = std::fs::metadata(&wal).unwrap().len();
        OpenOptions::new().write(true).open(&wal).unwrap().set_len(len - 3).unwrap();
        let db = Db::open(config(dir.path())).unwrap();
        let got = db.select(&[Matcher::eq("pod", "a")], t0, t0 + 20_000, 10_000).unwrap();
        assert_eq!(got[0].samples, vec![(t0, 1.0)]);
    }

    #[test]
    fn seals_downsamples_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.tiers = vec![
            TierConfig { resolution: Duration::from_secs(10), retention: Duration::from_secs(2 * 3600) },
            TierConfig { resolution: Duration::from_secs(60), retention: Duration::from_secs(2 * 86400) },
        ];
        let db = Db::open(cfg.clone()).unwrap();
        // Six hours of a counter and a gauge, ending now.
        let now = now_ms() / 60_000 * 60_000;
        let t0 = now - 6 * 3600 * 1000;
        for i in 0..(6 * 360) {
            let t = t0 + i * 10_000;
            db.append("n1", t, vec![(labels("c_total", "a"), i as f64), (labels("g", "a"), (i % 6) as f64)]).unwrap();
            if i % 60 == 0 {
                db.maintain(t).unwrap();
            }
        }
        db.maintain(now).unwrap();
        let st = db.stats();
        assert!(st.tiers[0].blocks >= 1 && st.tiers[1].blocks >= 1, "{st:?}");
        // The start is past raw retention: the 1m tier answers, downsampled.
        let g = db.select(&[Matcher::eq("__name__", "g")], t0, now, 60_000).unwrap();
        let first = g[0].samples[0];
        assert_eq!(first.1, 2.5, "average of 0..5");
        assert_eq!(first.0 % 60_000, 0);
        let c = db.select(&[Matcher::eq("__name__", "c_total")], t0, now, 60_000).unwrap();
        assert_eq!(c[0].samples[0].1, 5.0, "last of the bucket");
        // ~360 one-minute points over six hours, recent part filled from raw.
        assert!((350..=362).contains(&c[0].samples.len()), "{}", c[0].samples.len());
        assert!(c[0].samples.last().unwrap().0 >= now - 120_000);
        // Recent and fine: raw.
        let recent = db.select(&[Matcher::eq("__name__", "g")], now - 600_000, now, 10_000).unwrap();
        assert_eq!(recent[0].samples.len(), 60);
        // Still the same after a restart.
        drop(db);
        let db = Db::open(cfg).unwrap();
        let c2 = db.select(&[Matcher::eq("__name__", "c_total")], t0, now, 60_000).unwrap();
        assert_eq!(c[0].samples, c2[0].samples);
        // Retention: a day later the raw tier has dropped old blocks.
        db.maintain(now + 86_400_000).unwrap();
        let st = db.stats();
        assert_eq!(st.tiers[0].blocks, 0, "{st:?}");
        assert!(st.tiers[1].blocks >= 1);
    }

    #[test]
    fn refuses_series_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.max_series = 3;
        let db = Db::open(cfg).unwrap();
        let t = now_ms();
        db.append("n", t, (0..5).map(|i| (labels("x", &i.to_string()), 1.0)).collect()).unwrap();
        let st = db.stats();
        assert_eq!((st.series, st.series_refused), (3, 2));
    }

    #[test]
    fn downsample_buckets() {
        let s = vec![(0, 1.0), (30_000, 3.0), (60_000, 10.0), (61_000, stale()), (125_000, 4.0)];
        assert_eq!(downsample(&s, 60_000, false), vec![(0, 2.0), (60_000, 10.0), (120_000, 4.0)]);
        assert_eq!(downsample(&s, 60_000, true), vec![(0, 3.0), (60_000, 10.0), (120_000, 4.0)]);
    }
}
