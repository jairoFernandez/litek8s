//! Alerting: rules (built in, and user ones as `AlertRule` objects, with
//! Prometheus rule syntax; `PrometheusRule` objects are read too when that
//! CRD is installed) evaluated every interval against the store; alerts go
//! pending → firing after their `for`, resolve when the expression stops
//! returning them. `AlertReceiver` objects say where notifications go
//! (webhook, ntfy, Telegram); `Silence` objects mute matching alerts (an
//! *ack* is a silence on one alert that ends when it resolves).
//!
//! State (which alerts are active since when, who was notified) is kept in
//! `alerts.json` next to the metrics, so a restart neither forgets an
//! alert nor notifies it again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::notify::{Note, Target};
use super::promql::{self, Value as QValue};
use super::tsdb::Db;
use crate::kube::store::Store;

pub const GROUP: &str = "observability.litek8s.dev";
/// Annotation of a silence that ends when its alert resolves (an ack).
pub const UNTIL_RESOLVED: &str = "observability.litek8s.dev/until-resolved";
/// Resolved alerts stay listed this long.
const KEEP_RESOLVED_MS: i64 = 15 * 60 * 1000;
const DEFAULT_REPEAT_MS: i64 = 4 * 3600 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rule {
    pub name: String,
    pub expr: String,
    pub for_ms: i64,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    /// `builtin` or `<namespace>/<object name>`.
    pub source: String,
}

fn rule(name: &str, expr: &str, for_secs: i64, severity: &str, summary: &str, description: &str) -> Rule {
    Rule {
        name: name.into(),
        expr: expr.into(),
        for_ms: for_secs * 1000,
        labels: [("severity".to_string(), severity.to_string())].into(),
        annotations: [("summary".to_string(), summary.to_string()), ("description".to_string(), description.to_string())].into(),
        source: "builtin".into(),
    }
}

/// The rules every cluster gets (`--obs-builtin-alerts=false` turns them
/// off; silence single ones instead).
pub fn builtin_rules() -> Vec<Rule> {
    vec![
        rule(
            "KubePodCrashLooping",
            r#"max_over_time(kube_pod_container_status_waiting_reason{reason="CrashLoopBackOff"}[5m]) >= 1"#,
            60,
            "warning",
            "{{ $labels.namespace }}/{{ $labels.pod }} ({{ $labels.container }}) is crash looping",
            "Container {{ $labels.container }} of pod {{ $labels.namespace }}/{{ $labels.pod }} keeps failing and is waiting in CrashLoopBackOff.",
        ),
        rule(
            "KubeContainerOOMKilled",
            r#"kube_pod_container_status_last_terminated_reason{reason="OOMKilled"} == 1 and on(namespace, pod, container) increase(kube_pod_container_status_restarts_total[10m]) > 0"#,
            0,
            "warning",
            "{{ $labels.namespace }}/{{ $labels.pod }} ({{ $labels.container }}) was OOM killed",
            "Container {{ $labels.container }} of pod {{ $labels.namespace }}/{{ $labels.pod }} ran out of memory (its limit or the node's) and was killed in the last 10 minutes.",
        ),
        rule(
            "KubePodRestartLoop",
            "increase(kube_pod_container_status_restarts_total[15m]) > 3",
            0,
            "warning",
            "{{ $labels.namespace }}/{{ $labels.pod }} ({{ $labels.container }}) restarted {{ $value | humanize }} times in 15 minutes",
            "Container {{ $labels.container }} of pod {{ $labels.namespace }}/{{ $labels.pod }} restarts again and again.",
        ),
        rule(
            "KubePodNotReady",
            r#"sum by (namespace, pod) (kube_pod_status_phase{phase=~"Pending|Unknown"}) > 0"#,
            900,
            "warning",
            "{{ $labels.namespace }}/{{ $labels.pod }} has not been ready for 15 minutes",
            "Pod {{ $labels.namespace }}/{{ $labels.pod }} is Pending or Unknown (unschedulable, image pull, or its node is gone).",
        ),
        rule(
            "KubeNodeNotReady",
            r#"kube_node_status_condition{condition="Ready",status="true"} == 0"#,
            120,
            "critical",
            "Node {{ $labels.node }} is not ready",
            "Node {{ $labels.node }} has not reported as ready for 2 minutes: it stopped heartbeating or reports a problem.",
        ),
        rule(
            "KubeNodePressure",
            r#"kube_node_status_condition{condition=~"DiskPressure|MemoryPressure|PIDPressure",status="true"} == 1"#,
            120,
            "warning",
            "Node {{ $labels.node }} reports {{ $labels.condition }}",
            "Node {{ $labels.node }} reports {{ $labels.condition }}: pods may be evicted.",
        ),
        rule(
            "NodeFilesystemAlmostFull",
            "node_filesystem_avail_bytes / node_filesystem_size_bytes < 0.10",
            300,
            "warning",
            "Node {{ $labels.node }} has {{ $value | humanizePercentage }} disk left on {{ $labels.mountpoint }}",
            "The filesystem holding litek8s's data on {{ $labels.node }} is almost full: images, logs and volumes will fail to write.",
        ),
        rule(
            "NodeMemoryLow",
            "node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes < 0.10",
            300,
            "warning",
            "Node {{ $labels.node }} has {{ $value | humanizePercentage }} memory available",
            "Node {{ $labels.node }} is close to running out of memory: the kernel will start OOM killing.",
        ),
        rule(
            "KubePersistentVolumeFillingUp",
            "kubelet_volume_stats_available_bytes / kubelet_volume_stats_capacity_bytes < 0.15",
            120,
            "warning",
            "Volume {{ $labels.namespace }}/{{ $labels.persistentvolumeclaim }} has {{ $value | humanizePercentage }} space left",
            "The persistent volume claimed by {{ $labels.namespace }}/{{ $labels.persistentvolumeclaim }} on {{ $labels.node }} is almost full.",
        ),
        rule(
            "Litek8sMetricsSeriesRefused",
            "increase(litek8s_tsdb_series_refused_total[10m]) > 0",
            0,
            "warning",
            "litek8s's metrics store is full and refuses new series",
            "The time-series store reached its series limit (--obs-memory): raise it, or find what creates so many series.",
        ),
    ]
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect())
        .unwrap_or_default()
}

fn duration_ms(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_str).and_then(|s| super::tsdb::parse_duration(s).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Rules of `AlertRule` (and `PrometheusRule`) objects:
/// `spec.groups[].rules[]` with `alert`, `expr`, `for`, `labels`,
/// `annotations` (recording rules are skipped).
pub fn object_rules(store: &Store) -> Vec<Rule> {
    let mut out = Vec::new();
    for (group, plural) in [(GROUP, "alertrules"), ("monitoring.coreos.com", "prometheusrules")] {
        let (objects, _) = store.list(group, plural, None);
        for o in &objects {
            let source = format!("{}/{}", o.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default(), o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default());
            for g in o.pointer("/spec/groups").and_then(Value::as_array).into_iter().flatten() {
                for r in g.get("rules").and_then(Value::as_array).into_iter().flatten() {
                    let (Some(name), Some(expr)) = (r.get("alert").and_then(Value::as_str), r.get("expr")) else { continue };
                    let expr = expr.as_str().map(str::to_string).unwrap_or_else(|| expr.to_string());
                    out.push(Rule { name: name.into(), expr, for_ms: duration_ms(r.get("for")), labels: string_map(r.get("labels")), annotations: string_map(r.get("annotations")), source: source.clone() });
                }
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SilenceMatcher {
    pub name: String,
    pub value: String,
    #[serde(default, rename = "isRegex")]
    pub is_regex: bool,
    #[serde(default = "yes", rename = "isEqual")]
    pub is_equal: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq)]
pub struct Silence {
    pub name: String,
    pub matchers: Vec<SilenceMatcher>,
    pub starts_at: i64,
    pub ends_at: i64,
    pub until_resolved: bool,
    pub comment: String,
}

fn parse_time(v: Option<&Value>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v?.as_str()?).ok().map(|t| t.timestamp_millis())
}

pub fn silences(store: &Store) -> Vec<Silence> {
    let (objects, _) = store.list(GROUP, "silences", None);
    objects
        .iter()
        .map(|o| Silence {
            name: o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
            matchers: o.pointer("/spec/matchers").and_then(|m| serde_json::from_value(m.clone()).ok()).unwrap_or_default(),
            starts_at: parse_time(o.pointer("/spec/startsAt")).unwrap_or(0),
            ends_at: parse_time(o.pointer("/spec/endsAt")).unwrap_or(i64::MAX),
            until_resolved: o.pointer(&format!("/metadata/annotations/{}", UNTIL_RESOLVED.replace('/', "~1"))).and_then(Value::as_str) == Some("true"),
            comment: o.pointer("/spec/comment").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
        .collect()
}

impl Silence {
    pub fn active(&self, now: i64) -> bool {
        self.starts_at <= now && now < self.ends_at
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        !self.matchers.is_empty()
            && self.matchers.iter().all(|m| {
                let v = labels.get(&m.name).map(String::as_str).unwrap_or("");
                let hit = if m.is_regex { regex::Regex::new(&format!("^(?:{})$", m.value)).is_ok_and(|r| r.is_match(v)) } else { v == m.value };
                hit == m.is_equal
            })
    }
}

#[derive(Clone, Debug)]
pub struct Receiver {
    pub name: String,
    pub target: Target,
    /// Only alerts matching all of these.
    pub matchers: Vec<SilenceMatcher>,
    pub send_resolved: bool,
    pub repeat_ms: i64,
}

fn secret(store: &Store, r: Option<&Value>) -> Option<String> {
    let r = r?;
    let (ns, name, key) = (r.get("namespace")?.as_str()?, r.get("name")?.as_str()?, r.get("key")?.as_str()?);
    let s = store.get(&crate::kube::store::ObjectKey::new("", "secrets", ns, name))?;
    if let Some(v) = s.pointer(&format!("/stringData/{key}")).and_then(Value::as_str) {
        return Some(v.to_string());
    }
    let b64 = s.pointer(&format!("/data/{key}"))?.as_str()?;
    use base64::Engine;
    String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).ok()?).ok().map(|s| s.trim().to_string())
}

/// `AlertReceiver` objects, secrets resolved. Errors name the receiver.
pub fn receivers(store: &Store) -> (Vec<Receiver>, Vec<String>) {
    let (objects, _) = store.list(GROUP, "alertreceivers", None);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for o in &objects {
        let name = o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
        let spec = o.get("spec").cloned().unwrap_or_default();
        let target = if let Some(w) = spec.get("webhook") {
            w.get("url").and_then(Value::as_str).map(|url| Target::Webhook { url: url.into(), bearer: secret(store, w.get("bearerTokenSecretRef")) })
        } else if let Some(n) = spec.get("ntfy") {
            n.get("topic").and_then(Value::as_str).map(|topic| Target::Ntfy {
                url: n.get("url").and_then(Value::as_str).unwrap_or("https://ntfy.sh").to_string(),
                topic: topic.into(),
                token: secret(store, n.get("tokenSecretRef")),
            })
        } else if let Some(t) = spec.get("telegram") {
            match (secret(store, t.get("botTokenSecretRef")), t.get("chatId")) {
                (Some(token), Some(chat)) => Some(Target::Telegram {
                    token,
                    chat_id: chat.as_str().map(str::to_string).unwrap_or_else(|| chat.to_string()),
                    api: t.get("apiURL").and_then(Value::as_str).unwrap_or("https://api.telegram.org").to_string(),
                }),
                _ => None,
            }
        } else {
            None
        };
        let Some(target) = target else {
            errors.push(format!("receiver {name}: needs webhook.url, ntfy.topic or telegram.botTokenSecretRef + chatId (and the secret)"));
            continue;
        };
        out.push(Receiver {
            name,
            target,
            matchers: spec.get("matchers").and_then(|m| serde_json::from_value(m.clone()).ok()).unwrap_or_default(),
            send_resolved: spec.get("sendResolved").and_then(Value::as_bool).unwrap_or(true),
            repeat_ms: Some(duration_ms(spec.get("repeatInterval"))).filter(|d| *d > 0).unwrap_or(DEFAULT_REPEAT_MS),
        });
    }
    (out, errors)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Firing,
    Resolved,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alert {
    pub fingerprint: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub state: State,
    pub active_at: i64,
    #[serde(default)]
    pub fired_at: Option<i64>,
    #[serde(default)]
    pub resolved_at: Option<i64>,
    pub value: f64,
    /// receiver → last firing notification (ms)
    #[serde(default)]
    pub notified: BTreeMap<String, i64>,
    #[serde(default)]
    pub notified_resolved: BTreeSet<String>,
    /// Names of the silences muting it (computed each evaluation).
    #[serde(default)]
    pub silenced_by: Vec<String>,
}

fn fingerprint(labels: &BTreeMap<String, String>) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for (k, v) in labels {
        h.update(k.as_bytes());
        h.update([0xff]);
        h.update(v.as_bytes());
        h.update([0xfe]);
    }
    hex::encode(&h.finalize()[..8])
}

pub fn humanize(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return super::promql::format_value(v);
    }
    let units = ["", "k", "M", "G", "T", "P"];
    let mut x = v;
    let mut i = 0;
    while x.abs() >= 1000.0 && i < units.len() - 1 {
        x /= 1000.0;
        i += 1;
    }
    let s = format!("{x:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{}", units[i])
}

/// `{{ $labels.x }}`, `{{ $value }}` with optional `| humanize`,
/// `| humanizePercentage`, `| humanize1024`.
pub fn expand(template: &str, labels: &BTreeMap<String, String>, value: f64) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\{\{-?\s*(\$labels\.([A-Za-z_][A-Za-z0-9_]*)|\$value)\s*(?:\|\s*([A-Za-z0-9]+)\s*)?-?\}\}").unwrap());
    re.replace_all(template, |c: &regex::Captures| {
        if let Some(name) = c.get(2) {
            return labels.get(name.as_str()).cloned().unwrap_or_default();
        }
        match c.get(3).map(|m| m.as_str()) {
            Some("humanize") => humanize(value),
            Some("humanizePercentage") => format!("{}%", humanize(value * 100.0)),
            Some("humanize1024") => {
                let units = ["", "Ki", "Mi", "Gi", "Ti"];
                let (mut x, mut i) = (value, 0);
                while x.abs() >= 1024.0 && i < units.len() - 1 {
                    x /= 1024.0;
                    i += 1;
                }
                format!("{:.4}{}", x, units[i]).replace(".0000", "")
            }
            _ => super::promql::format_value(value),
        }
    })
    .into_owned()
}

pub struct Alerts {
    path: PathBuf,
    state: Mutex<BTreeMap<String, Alert>>,
    /// Rule (source/name) → its last evaluation error.
    errors: Mutex<BTreeMap<String, String>>,
    rules: Mutex<Vec<Rule>>,
    /// receiver → (failures in a row, don't retry before (ms), last error)
    backoff: Mutex<HashMap<String, (u32, i64, String)>>,
}

/// One notification to send, and what to record once it went out.
pub struct Outgoing {
    pub receiver: Receiver,
    pub notes: Vec<Note>,
    pub marks: Vec<(String, bool)>,
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl Alerts {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("alerts.json");
        let state = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Alerts { path, state: Mutex::new(state), errors: Mutex::default(), rules: Mutex::default(), backoff: Mutex::default() }
    }

    fn save(&self, state: &BTreeMap<String, Alert>) {
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec(state).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn snapshot(&self) -> Vec<Alert> {
        self.state.lock().unwrap().values().cloned().collect()
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.lock().unwrap().clone()
    }

    pub fn errors(&self) -> BTreeMap<String, String> {
        self.errors.lock().unwrap().clone()
    }

    pub fn receiver_errors(&self) -> BTreeMap<String, String> {
        self.backoff.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.2.clone())).collect()
    }

    /// Evaluate every rule at `now`; returns the silences to delete (acks
    /// whose alert resolved).
    pub fn evaluate(&self, db: &Db, rules: Vec<Rule>, silences: &[Silence], now: i64) -> Vec<String> {
        let mut seen: BTreeMap<String, (BTreeMap<String, String>, BTreeMap<String, String>, f64, i64)> = BTreeMap::new();
        let mut errors = BTreeMap::new();
        for r in &rules {
            let result = match promql::instant(db, &r.expr, now) {
                Ok(QValue::Vector(v)) => v.into_iter().map(|s| (s.labels, s.v)).collect::<Vec<_>>(),
                Ok(QValue::Scalar(v)) => vec![(Vec::new(), v)],
                Ok(other) => {
                    errors.insert(format!("{}/{}", r.source, r.name), format!("rule returned a {}", other.type_name()));
                    continue;
                }
                Err(e) => {
                    errors.insert(format!("{}/{}", r.source, r.name), format!("{e:#}"));
                    continue;
                }
            };
            for (labels, value) in result {
                let mut l: BTreeMap<String, String> = labels.into_iter().filter(|(k, _)| k != "__name__").collect();
                for (k, v) in &r.labels {
                    l.insert(k.clone(), expand(v, &l, value));
                }
                l.insert("alertname".into(), r.name.clone());
                let annotations = r.annotations.iter().map(|(k, v)| (k.clone(), expand(v, &l, value))).collect();
                seen.insert(fingerprint(&l), (l, annotations, value, r.for_ms));
            }
        }
        *self.errors.lock().unwrap() = errors;
        *self.rules.lock().unwrap() = rules;
        let mut state = self.state.lock().unwrap();
        let before = serde_json::to_vec(&*state).unwrap_or_default();
        for (fp, (labels, annotations, value, for_ms)) in &seen {
            let a = state.entry(fp.clone()).or_insert_with(|| Alert {
                fingerprint: fp.clone(),
                labels: labels.clone(),
                annotations: BTreeMap::new(),
                state: State::Pending,
                active_at: now,
                fired_at: None,
                resolved_at: None,
                value: *value,
                notified: BTreeMap::new(),
                notified_resolved: BTreeSet::new(),
                silenced_by: Vec::new(),
            });
            if a.state == State::Resolved {
                // Back again: a new episode.
                *a = Alert { state: State::Pending, active_at: now, fired_at: None, resolved_at: None, notified: BTreeMap::new(), notified_resolved: BTreeSet::new(), ..a.clone() };
            }
            a.value = *value;
            a.annotations = annotations.clone();
            if a.state == State::Pending && now - a.active_at >= *for_ms {
                a.state = State::Firing;
                a.fired_at = Some(now);
            }
        }
        let mut done_acks = Vec::new();
        state.retain(|fp, a| {
            if seen.contains_key(fp) {
                return true;
            }
            match a.state {
                State::Pending => false,
                State::Firing => {
                    a.state = State::Resolved;
                    a.resolved_at = Some(now);
                    true
                }
                State::Resolved => a.resolved_at.is_some_and(|r| now - r < KEEP_RESOLVED_MS),
            }
        });
        for a in state.values_mut() {
            a.silenced_by = silences.iter().filter(|s| s.active(now) && s.matches(&a.labels)).map(|s| s.name.clone()).collect();
            if a.state == State::Resolved {
                for s in silences.iter().filter(|s| s.until_resolved && s.matches(&a.labels)) {
                    done_acks.push(s.name.clone());
                }
            }
        }
        // Expired silences are not deleted (they document what was muted);
        // acks go away with their alert.
        done_acks.sort();
        done_acks.dedup();
        let after = serde_json::to_vec(&*state).unwrap_or_default();
        if after != before {
            self.save(&state);
        }
        done_acks
    }

    /// What to send to each receiver now.
    pub fn outgoing(&self, receivers: &[Receiver], now: i64) -> Vec<Outgoing> {
        let state = self.state.lock().unwrap();
        let backoff = self.backoff.lock().unwrap();
        let mut out = Vec::new();
        for r in receivers {
            if backoff.get(&r.name).is_some_and(|b| b.1 > now) {
                continue;
            }
            let mut notes = Vec::new();
            let mut marks = Vec::new();
            for a in state.values() {
                if !a.silenced_by.is_empty() || !r.matchers.iter().all(|m| Silence { name: String::new(), matchers: vec![m.clone()], starts_at: 0, ends_at: i64::MAX, until_resolved: false, comment: String::new() }.matches(&a.labels)) {
                    continue;
                }
                let note = |firing: bool| Note {
                    firing,
                    labels: a.labels.clone(),
                    annotations: a.annotations.clone(),
                    starts_at: rfc3339(a.fired_at.unwrap_or(a.active_at)),
                    ends_at: a.resolved_at.map(rfc3339).unwrap_or_else(|| "0001-01-01T00:00:00Z".into()),
                    fingerprint: a.fingerprint.clone(),
                };
                match a.state {
                    State::Firing if a.notified.get(&r.name).is_none_or(|t| now - t >= r.repeat_ms) => {
                        notes.push(note(true));
                        marks.push((a.fingerprint.clone(), true));
                    }
                    State::Resolved if r.send_resolved && a.notified.contains_key(&r.name) && !a.notified_resolved.contains(&r.name) => {
                        notes.push(note(false));
                        marks.push((a.fingerprint.clone(), false));
                    }
                    _ => {}
                }
            }
            if !notes.is_empty() {
                out.push(Outgoing { receiver: r.clone(), notes, marks });
            }
        }
        out
    }

    /// Record the result of sending one [`Outgoing`].
    pub fn sent(&self, o: &Outgoing, result: Result<(), String>, now: i64) {
        let mut backoff = self.backoff.lock().unwrap();
        match result {
            Ok(()) => {
                backoff.remove(&o.receiver.name);
                let mut state = self.state.lock().unwrap();
                for (fp, firing) in &o.marks {
                    if let Some(a) = state.get_mut(fp) {
                        if *firing {
                            a.notified.insert(o.receiver.name.clone(), now);
                        } else {
                            a.notified_resolved.insert(o.receiver.name.clone());
                        }
                    }
                }
                self.save(&state);
            }
            Err(e) => {
                let b = backoff.entry(o.receiver.name.clone()).or_insert((0, 0, String::new()));
                b.0 += 1;
                b.1 = now + (30_000i64 << b.0.min(5)).min(600_000);
                b.2 = e;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::tsdb::{Config, Labels};

    fn db(dir: &Path) -> Db {
        Db::open(Config { dir: Some(dir.to_path_buf()), ..Config::default() }).unwrap()
    }

    fn l(pairs: &[(&str, &str)]) -> Labels {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn templates() {
        let labels: BTreeMap<String, String> = [("pod".to_string(), "web".to_string())].into();
        assert_eq!(expand("{{ $labels.pod }} at {{ $value }}", &labels, 3.0), "web at 3");
        assert_eq!(expand("{{$value | humanize}}", &labels, 1_500_000.0), "1.5M");
        assert_eq!(expand("{{ $value | humanizePercentage }}", &labels, 0.051), "5.1%");
        assert_eq!(expand("{{ $labels.nope }}x", &labels, 0.0), "x");
    }

    #[test]
    fn builtin_rules_parse() {
        for r in builtin_rules() {
            promql::parse(&r.expr).unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
    }

    #[test]
    fn pending_firing_resolved_and_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let alerts = Alerts::load(dir.path());
        let t0 = crate::obs::tsdb::now_ms() - 600_000;
        let rule = Rule { for_ms: 60_000, source: "test".into(), ..rule("Down", "up == 0", 0, "critical", "{{ $labels.instance }} down", "") };
        let receiver = Receiver { name: "r".into(), target: Target::Webhook { url: "http://x".into(), bearer: None }, matchers: vec![], send_resolved: true, repeat_ms: 3_600_000 };
        let up = |t: i64, v: f64| db.append("s", t, vec![(l(&[("__name__", "up"), ("instance", "a")]), v)]).unwrap();
        up(t0, 0.0);
        alerts.evaluate(&db, vec![rule.clone()], &[], t0);
        assert_eq!(alerts.snapshot()[0].state, State::Pending);
        assert!(alerts.outgoing(&[receiver.clone()], t0).is_empty());
        up(t0 + 70_000, 0.0);
        alerts.evaluate(&db, vec![rule.clone()], &[], t0 + 70_000);
        let a = &alerts.snapshot()[0];
        assert_eq!(a.state, State::Firing);
        assert_eq!(a.annotations["summary"], "a down");
        let out = alerts.outgoing(&[receiver.clone()], t0 + 70_000);
        assert_eq!(out[0].notes.len(), 1);
        alerts.sent(&out[0], Ok(()), t0 + 70_000);
        assert!(alerts.outgoing(&[receiver.clone()], t0 + 80_000).is_empty(), "no repeat before the interval");
        // A silence mutes it.
        let s = Silence { name: "s1".into(), matchers: vec![SilenceMatcher { name: "instance".into(), value: "a".into(), is_regex: false, is_equal: true }], starts_at: 0, ends_at: i64::MAX, until_resolved: true, comment: String::new() };
        up(t0 + 80_000, 0.0);
        alerts.evaluate(&db, vec![rule.clone()], std::slice::from_ref(&s), t0 + 80_000);
        assert_eq!(alerts.snapshot()[0].silenced_by, vec!["s1"]);
        // Resolves; the ack goes away; resolved notification (silenced
        // alerts are not notified, so drop the silence first).
        up(t0 + 90_000, 1.0);
        let done = alerts.evaluate(&db, vec![rule.clone()], std::slice::from_ref(&s), t0 + 90_000);
        assert_eq!(done, vec!["s1"]);
        alerts.evaluate(&db, vec![rule.clone()], &[], t0 + 90_000);
        let a = &alerts.snapshot()[0];
        assert_eq!(a.state, State::Resolved);
        let out = alerts.outgoing(&[receiver.clone()], t0 + 90_000);
        assert!(!out[0].notes[0].firing);
        alerts.sent(&out[0], Err("boom".into()), t0 + 90_000);
        assert!(alerts.outgoing(&[receiver.clone()], t0 + 95_000).is_empty(), "backoff");
        // State survives a restart.
        let again = Alerts::load(dir.path());
        assert_eq!(again.snapshot()[0].state, State::Resolved);
    }

    #[test]
    fn silence_matching() {
        let labels: BTreeMap<String, String> = [("alertname", "X"), ("namespace", "prod")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let m = |name: &str, value: &str, re: bool, eq: bool| SilenceMatcher { name: name.into(), value: value.into(), is_regex: re, is_equal: eq };
        let s = |ms| Silence { name: "s".into(), matchers: ms, starts_at: 0, ends_at: 10, until_resolved: false, comment: String::new() };
        assert!(s(vec![m("namespace", "pro.*", true, true)]).matches(&labels));
        assert!(!s(vec![m("namespace", "prod", false, false)]).matches(&labels));
        assert!(!s(vec![]).matches(&labels));
        assert!(!s(vec![m("alertname", "X", false, true)]).active(10));
    }
}
