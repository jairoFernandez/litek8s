//! Log search across every pod and node, without a log store of its own.
//!
//! Each node agent already writes container output to CRI-format files
//! (`<time> <stream> <F|P> <text>`, rotated at 10 MiB, see
//! [`crate::kube::agent::crun::logs`]). A query goes to every node; each
//! agent filters its own files (namespace, pod label selector, pod name
//! regex, container, time range, a regex on the text), keeps the newest
//! `limit` matches and streams them back as JSON lines; the API server
//! merges them by time. `follow` keeps tailing the files (and picks up
//! containers that start later).
//!
//! Logs outlive their container: when the crun runtime removes one (its pod
//! was deleted, or it was restarted twice), the files move to the node's
//! archive (`<node dir>/logs/<namespace>/<pod>/`), which is searched too and
//! kept within a retention time and a size cap ([`sweep`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::kube::agent::crun::logs as cri;
use crate::kube::agent::runtime::{LABEL_ATTEMPT, LABEL_CONTAINER, LABEL_POD_NAME, LABEL_POD_NAMESPACE, LABEL_POD_UID, LABEL_ROLE};

/// Container label holding the pod's labels (JSON), so archived logs can
/// still be found by label selector after the pod is gone.
pub const LABEL_POD_LABELS: &str = "litek8s.pod.labels";

pub const DEFAULT_LIMIT: usize = 1000;
/// Largest `limit` a query may ask for.
pub const MAX_LIMIT: usize = 100_000;

/// What to search for (the query string of `GET /litek8s/v1/logs`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LogQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// A label selector on pods (`app=web,tier!=db`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// A regex the pod name must contain a match of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Only lines at or after this (unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    /// A regex the line must contain a match of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
    /// The newest this many lines (of the history; follow adds more).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub follow: bool,
    /// Leave out logs of containers that are gone (the archive).
    #[serde(default)]
    pub live_only: bool,
}

/// One line of output.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogLine {
    /// RFC 3339 UTC with nanoseconds: sorts as text.
    pub ts: String,
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub node: String,
    pub stream: String,
    pub line: String,
}

/// A query ready to run.
pub struct Compiled {
    pub q: LogQuery,
    selector: Option<crate::kube::selector::LabelSelector>,
    pod: Option<regex::Regex>,
    grep: Option<regex::Regex>,
    since: Option<String>,
    until: Option<String>,
    pub limit: usize,
}

fn ts_string(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// A CRI or Docker timestamp in the sortable form.
pub fn normalize_ts(ts: &str) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(t.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
}

impl Compiled {
    pub fn new(q: LogQuery) -> Result<Self> {
        let selector = match q.selector.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => Some(crate::kube::selector::LabelSelector::parse(s).map_err(|e| anyhow::anyhow!("bad selector {s:?}: {e:?}"))?),
            None => None,
        };
        let pod = q.pod.as_deref().filter(|p| !p.is_empty()).map(regex::Regex::new).transpose().context("bad pod regex")?;
        let grep = q.grep.as_deref().filter(|g| !g.is_empty()).map(regex::Regex::new).transpose().context("bad grep regex")?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        Ok(Compiled { since: q.since_ms.map(ts_string), until: q.until_ms.map(ts_string), selector, pod, grep, limit, q })
    }

    /// Whether a container's logs are wanted at all.
    pub fn wants(&self, namespace: &str, pod: &str, container: &str, labels: &BTreeMap<String, String>) -> bool {
        if self.q.namespace.as_deref().is_some_and(|n| !n.is_empty() && n != namespace) {
            return false;
        }
        if self.q.container.as_deref().is_some_and(|c| !c.is_empty() && c != container) {
            return false;
        }
        if self.pod.as_ref().is_some_and(|r| !r.is_match(pod)) {
            return false;
        }
        if let Some(sel) = &self.selector {
            if !sel.matches(&serde_json::json!({"metadata": {"labels": labels}})) {
                return false;
            }
        }
        true
    }

    /// Whether a line (its normalized time and text) is wanted.
    pub fn wants_line(&self, ts: &str, text: &str) -> bool {
        if self.since.as_deref().is_some_and(|s| ts < s) || self.until.as_deref().is_some_and(|u| ts > u) {
            return false;
        }
        self.grep_matches(text)
    }

    pub fn grep_matches(&self, text: &str) -> bool {
        self.grep.as_ref().is_none_or(|g| g.is_match(text))
    }
}

/// Where a container's logs are.
#[derive(Clone, Debug)]
pub struct Source {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub labels: BTreeMap<String, String>,
    /// CRI files, oldest first (the live one last).
    pub files: Vec<PathBuf>,
    /// Still running (followed when the query follows).
    pub live: bool,
}

/// Parses CRI records into whole lines (joining `P` records).
#[derive(Default)]
pub struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    /// Feed whole records; calls `f(ts, stream, text)` per complete line.
    pub fn feed(&mut self, data: &[u8], mut f: impl FnMut(&str, u8, &str)) {
        for rec in data.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let Some(r) = cri::parse(rec) else { continue };
            if !r.full {
                self.partial.extend_from_slice(r.content);
                continue;
            }
            let text = if self.partial.is_empty() {
                String::from_utf8_lossy(r.content).into_owned()
            } else {
                self.partial.extend_from_slice(r.content);
                let t = String::from_utf8_lossy(&self.partial).into_owned();
                self.partial.clear();
                t
            };
            f(r.time, r.stream, &text);
        }
    }
}

fn stream_name(s: u8) -> &'static str {
    if s == 2 { "stderr" } else { "stdout" }
}

impl Source {
    fn line(&self, node: &str, ts: String, stream: u8, text: &str) -> LogLine {
        LogLine {
            ts,
            namespace: self.namespace.clone(),
            pod: self.pod.clone(),
            container: self.container.clone(),
            node: node.to_string(),
            stream: stream_name(stream).into(),
            line: text.to_string(),
        }
    }

    /// The newest `q.limit` matching lines of this source.
    pub fn search(&self, q: &Compiled, node: &str) -> VecDeque<LogLine> {
        let mut out = VecDeque::new();
        let mut lines = Lines::default();
        for f in &self.files {
            let Ok(data) = std::fs::read(f) else { continue };
            let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
            lines.feed(&data[..complete], |ts, stream, text| {
                let Some(ts) = normalize_ts(ts) else { return };
                if q.wants_line(&ts, text) {
                    if out.len() == q.limit {
                        out.pop_front();
                    }
                    out.push_back(self.line(node, ts, stream, text));
                }
            });
        }
        out
    }
}

/// Merge per-source results (each sorted) into the newest `limit`, oldest
/// first.
pub fn merge(parts: Vec<VecDeque<LogLine>>, limit: usize) -> Vec<LogLine> {
    let mut all: Vec<LogLine> = parts.into_iter().flatten().collect();
    all.sort_by(|a, b| a.ts.cmp(&b.ts));
    if all.len() > limit {
        all.drain(..all.len() - limit);
    }
    all
}

/// Tails the live file of each followed source.
#[derive(Default)]
pub struct Follower {
    /// live file → (offset, line joiner)
    files: HashMap<PathBuf, (u64, Lines)>,
}

impl Follower {
    /// Start after what the history covered (sources known now), or from
    /// the beginning for sources that appear later.
    pub fn start(&mut self, sources: &[Source]) {
        for s in sources.iter().filter(|s| s.live) {
            if let Some(f) = s.files.last() {
                let len = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                self.files.insert(f.clone(), (len, Lines::default()));
            }
        }
    }

    /// New matching lines since the last poll.
    pub fn poll(&mut self, sources: &[Source], q: &Compiled, node: &str) -> Vec<LogLine> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for s in sources.iter().filter(|s| s.live) {
            let Some(f) = s.files.last() else { continue };
            seen.push(f.clone());
            let (offset, lines) = self.files.entry(f.clone()).or_insert_with(|| (0, Lines::default()));
            let Ok(mut file) = std::fs::File::open(f) else { continue };
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len < *offset {
                // Rotated: the old file became `log.1`; read the new one.
                *offset = 0;
            }
            if len == *offset {
                continue;
            }
            let mut data = Vec::new();
            if file.seek(SeekFrom::Start(*offset)).is_err() || file.take(len - *offset).read_to_end(&mut data).is_err() {
                continue;
            }
            let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
            *offset += complete as u64;
            lines.feed(&data[..complete], |ts, stream, text| {
                let Some(ts) = normalize_ts(ts) else { return };
                if q.grep.as_ref().is_none_or(|g| g.is_match(text)) {
                    out.push(s.line(node, ts, stream, text));
                }
            });
        }
        self.files.retain(|f, _| seen.contains(f));
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        out
    }
}

/// Pod labels as stored on a container (`litek8s.pod.labels`).
pub fn pod_labels(container_labels: &HashMap<String, String>) -> BTreeMap<String, String> {
    container_labels.get(LABEL_POD_LABELS).and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default()
}

/// Move a removed container's log files into the archive. Called by the
/// runtime just before it deletes the container's directory.
pub fn archive(archive_root: &Path, labels: &HashMap<String, String>, files: &[PathBuf]) -> Result<()> {
    if labels.get(LABEL_ROLE).is_some_and(|r| r == "sandbox") {
        return Ok(());
    }
    let get = |k: &str| labels.get(k).map(String::as_str).unwrap_or_default();
    let (ns, pod, uid, container) = (get(LABEL_POD_NAMESPACE), get(LABEL_POD_NAME), get(LABEL_POD_UID), get(LABEL_CONTAINER));
    if ns.is_empty() || pod.is_empty() || container.is_empty() || ns.contains('/') || pod.contains('/') || container.contains('/') {
        return Ok(());
    }
    let present: Vec<&PathBuf> = files.iter().filter(|f| std::fs::metadata(f).is_ok_and(|m| m.len() > 0)).collect();
    if present.is_empty() {
        return Ok(());
    }
    let dir = archive_root.join(ns).join(pod);
    std::fs::create_dir_all(&dir)?;
    let short_uid: String = uid.chars().filter(|c| *c != '-').take(8).collect();
    let dest = dir.join(format!("{container}.{}.{short_uid}.log", get(LABEL_ATTEMPT).parse::<u32>().unwrap_or(0)));
    // The first file is renamed (same filesystem), the rest appended.
    std::fs::rename(present[0], &dest).or_else(|_| std::fs::copy(present[0], &dest).map(|_| ()))?;
    if present.len() > 1 {
        let mut out = std::fs::OpenOptions::new().append(true).open(&dest)?;
        for f in &present[1..] {
            let mut src = std::fs::File::open(f)?;
            std::io::copy(&mut src, &mut out)?;
        }
        out.flush()?;
    }
    let meta = dir.join("labels.json");
    let pod_labels = pod_labels(labels);
    if !pod_labels.is_empty() {
        let _ = std::fs::write(meta, serde_json::to_vec(&pod_labels)?);
    }
    Ok(())
}

/// The archive's files as sources.
pub fn archived(archive_root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    let Ok(namespaces) = std::fs::read_dir(archive_root) else { return out };
    for ns in namespaces.flatten() {
        let Ok(pods) = std::fs::read_dir(ns.path()) else { continue };
        for pod in pods.flatten() {
            let labels: BTreeMap<String, String> = std::fs::read(pod.path().join("labels.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let Ok(files) = std::fs::read_dir(pod.path()) else { continue };
            let mut files: Vec<(SystemTime, PathBuf, String)> = files
                .flatten()
                .filter_map(|f| {
                    let name = f.file_name().to_string_lossy().to_string();
                    let container = name.strip_suffix(".log")?.split('.').next()?.to_string();
                    Some((f.metadata().ok()?.modified().ok()?, f.path(), container))
                })
                .collect();
            files.sort();
            for (_, path, container) in files {
                out.push(Source {
                    namespace: ns.file_name().to_string_lossy().into(),
                    pod: pod.file_name().to_string_lossy().into(),
                    container,
                    labels: labels.clone(),
                    files: vec![path],
                    live: false,
                });
            }
        }
    }
    out
}

/// Keep the archive within `retention` and `max_bytes` (oldest go first).
/// Returns the bytes it holds afterwards.
pub fn sweep(archive_root: &Path, retention: Duration, max_bytes: u64) -> u64 {
    let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    let now = SystemTime::now();
    for s in archived(archive_root) {
        for f in s.files {
            if let Ok(m) = std::fs::metadata(&f) {
                let modified = m.modified().unwrap_or(now);
                if now.duration_since(modified).unwrap_or_default() > retention {
                    let _ = std::fs::remove_file(&f);
                } else {
                    files.push((modified, f, m.len()));
                }
            }
        }
    }
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    for (_, f, len) in &files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(f).is_ok() {
            total -= len;
        }
    }
    // Drop pod directories left with only labels.json.
    if let Ok(namespaces) = std::fs::read_dir(archive_root) {
        for ns in namespaces.flatten() {
            if let Ok(pods) = std::fs::read_dir(ns.path()) {
                for pod in pods.flatten() {
                    let only_meta = std::fs::read_dir(pod.path()).map(|d| d.flatten().all(|f| f.file_name() == "labels.json")).unwrap_or(false);
                    if only_meta {
                        let _ = std::fs::remove_dir_all(pod.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(ns.path());
        }
    }
    total
}

/// `since` as given to the API: a duration back from now (`1h`) or an RFC
/// 3339 time.
pub fn parse_time(s: &str, now_ms: i64) -> Result<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(secs) = s.parse::<f64>() {
        // A unix time in seconds (as Prometheus takes them).
        if secs > 1e9 {
            return Ok((secs * 1000.0) as i64);
        }
    }
    let d = crate::obs::tsdb::parse_duration(s)?;
    Ok(now_ms - d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: &str, stream: &str, full: bool, text: &str) -> String {
        format!("{ts} {stream} {} {text}\n", if full { "F" } else { "P" })
    }

    fn source(dir: &Path, name: &str, pod: &str, labels: &[(&str, &str)], content: &str) -> Source {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        Source {
            namespace: "default".into(),
            pod: pod.into(),
            container: "app".into(),
            labels: labels.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            files: vec![path],
            live: true,
        }
    }

    #[test]
    fn searches_filters_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let a = source(
            dir.path(),
            "a",
            "web-1",
            &[("app", "web")],
            &[
                rec("2026-10-19T10:00:00.000000001Z", "stdout", true, "GET / 200"),
                rec("2026-10-19T10:00:02.000000000Z", "stderr", false, "ERROR part one "),
                rec("2026-10-19T10:00:02.000000000Z", "stderr", true, "and two"),
                rec("2026-10-19T10:00:04.000000000Z", "stdout", true, "GET /x 500"),
            ]
            .concat(),
        );
        let b = source(dir.path(), "b", "db-0", &[("app", "db")], &rec("2026-10-19T10:00:03.000000000Z", "stdout", true, "ERROR db"));
        let q = Compiled::new(LogQuery { grep: Some("ERROR|500".into()), ..Default::default() }).unwrap();
        let merged = merge(vec![a.search(&q, "n1"), b.search(&q, "n2")], 10);
        assert_eq!(merged.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(), vec!["ERROR part one and two", "ERROR db", "GET /x 500"]);
        assert_eq!(merged[0].stream, "stderr");
        assert_eq!(merged[1].node, "n2");
        // Limit keeps the newest.
        assert_eq!(merge(vec![a.search(&q, "n1"), b.search(&q, "n2")], 1)[0].line, "GET /x 500");
        // Selector and time range.
        let q = Compiled::new(LogQuery { selector: Some("app=web".into()), since_ms: Some(chrono::DateTime::parse_from_rfc3339("2026-10-19T10:00:03Z").unwrap().timestamp_millis()), ..Default::default() }).unwrap();
        assert!(q.wants("default", "web-1", "app", &a.labels));
        assert!(!q.wants("default", "db-0", "app", &b.labels));
        assert_eq!(a.search(&q, "n1").len(), 1);
        let q = Compiled::new(LogQuery { namespace: Some("other".into()), ..Default::default() }).unwrap();
        assert!(!q.wants("default", "web-1", "app", &a.labels));
        let q = Compiled::new(LogQuery { pod: Some("^db".into()), ..Default::default() }).unwrap();
        assert!(q.wants("default", "db-0", "app", &b.labels) && !q.wants("default", "web-1", "app", &a.labels));
    }

    #[test]
    fn follows_appends_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let s = source(dir.path(), "log", "p", &[], &rec("2026-10-19T10:00:00Z", "stdout", true, "old"));
        let q = Compiled::new(LogQuery { follow: true, ..Default::default() }).unwrap();
        let mut f = Follower::default();
        f.start(std::slice::from_ref(&s));
        assert!(f.poll(std::slice::from_ref(&s), &q, "n").is_empty());
        let mut file = std::fs::OpenOptions::new().append(true).open(&s.files[0]).unwrap();
        file.write_all(rec("2026-10-19T10:00:01Z", "stdout", true, "new").as_bytes()).unwrap();
        file.write_all(b"2026-10-19T10:00:02Z stdout F incomplete").unwrap();
        let got = f.poll(std::slice::from_ref(&s), &q, "n");
        assert_eq!(got.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(), vec!["new"]);
        // Rotation: a shorter file starts over.
        std::fs::write(&s.files[0], rec("2026-10-19T10:00:03Z", "stdout", true, "rotated")).unwrap();
        assert_eq!(f.poll(std::slice::from_ref(&s), &q, "n")[0].line, "rotated");
    }

    #[test]
    fn archive_then_search_then_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("c");
        std::fs::create_dir(&c).unwrap();
        std::fs::write(c.join("log.1"), rec("2026-10-19T10:00:00Z", "stdout", true, "first")).unwrap();
        std::fs::write(c.join("log"), rec("2026-10-19T10:00:01Z", "stdout", true, "second")).unwrap();
        let labels: HashMap<String, String> = [
            (LABEL_POD_NAMESPACE, "ns"),
            (LABEL_POD_NAME, "job-x"),
            (LABEL_POD_UID, "1234-5678-9abc"),
            (LABEL_CONTAINER, "main"),
            (LABEL_ATTEMPT, "2"),
            (LABEL_POD_LABELS, r#"{"app":"batch"}"#),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let root = dir.path().join("logs");
        archive(&root, &labels, &[c.join("log.1"), c.join("log")]).unwrap();
        let sources = archived(&root);
        assert_eq!(sources.len(), 1);
        assert_eq!((sources[0].pod.as_str(), sources[0].container.as_str(), sources[0].labels["app"].as_str()), ("job-x", "main", "batch"));
        let q = Compiled::new(LogQuery { selector: Some("app=batch".into()), ..Default::default() }).unwrap();
        let lines = sources[0].search(&q, "n");
        assert_eq!(lines.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
        // Size cap: everything goes.
        assert_eq!(sweep(&root, Duration::from_secs(3600), 0), 0);
        assert!(archived(&root).is_empty());
        assert!(!root.join("ns").exists());
    }

    #[test]
    fn times() {
        let now = 1_000_000_000_000;
        assert_eq!(parse_time("1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse_time("2026-10-19T10:00:00Z", now).unwrap(), 1_792_404_000_000);
        assert_eq!(normalize_ts("2026-10-19T10:00:00.5Z").unwrap(), "2026-10-19T10:00:00.500000000Z");
    }
}
