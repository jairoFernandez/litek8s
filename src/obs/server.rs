//! The API server's side: the store, what it collects itself (object
//! state, its own request metrics, the store's own health), what node
//! agents send, alert evaluation and notifications, all on one interval.
//!
//! This is a module of its own with its own data directory (`<data
//! dir>/obs`), so it can follow the control plane's leader when the API
//! server runs replicated: only the instance that runs the controllers
//! should run [`run`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::alerts::{self, Alerts};
use super::tsdb::{self, Db, Labels, TierConfig};
use super::ObsSettings;
use crate::kube::server::ApiState;

#[derive(Clone, Debug)]
pub struct ObsConfig {
    /// `None`: a temporary directory (an API server without --data-dir).
    pub dir: Option<PathBuf>,
    pub tiers: Vec<TierConfig>,
    pub interval: Duration,
    /// What the store may hold in memory (series index and head chunks).
    pub memory_budget: u64,
    pub log_retention: Duration,
    pub log_max_bytes: u64,
    pub builtin_alerts: bool,
    pub eval_interval: Duration,
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            dir: None,
            tiers: tsdb::default_tiers(),
            interval: Duration::from_secs(10),
            memory_budget: 24 << 20,
            log_retention: Duration::from_secs(7 * 86400),
            log_max_bytes: 512 << 20,
            builtin_alerts: true,
            eval_interval: Duration::from_secs(30),
        }
    }
}

/// Bytes the store's index and heads take per series, measured (labels
/// of a cAdvisor container series, its postings, a head chunk of up to
/// one checkpoint of samples).
const BYTES_PER_SERIES: u64 = 900;

pub struct Obs {
    pub db: Db,
    pub alerts: Alerts,
    pub config: ObsConfig,
}

impl Obs {
    pub fn open(config: ObsConfig) -> Result<Self> {
        // A third of the budget is left for queries, the rest is series.
        let max_series = ((config.memory_budget * 2 / 3) / BYTES_PER_SERIES).max(1000) as usize;
        let db = Db::open(tsdb::Config { dir: config.dir.clone(), tiers: config.tiers.clone(), max_series, ..tsdb::Config::default() })?;
        let alerts = Alerts::load(db.dir());
        Ok(Obs { db, alerts, config })
    }

    pub fn settings(&self) -> ObsSettings {
        ObsSettings {
            interval_ms: self.config.interval.as_millis() as u64,
            log_retention_secs: self.config.log_retention.as_secs(),
            log_max_bytes: self.config.log_max_bytes,
        }
    }

    /// Samples a node agent sent. `ids` maps the connection's series ids to
    /// the store's (kept by the caller for the connection's life).
    pub fn ingest_node(&self, node: &str, ts: i64, series: Vec<(u32, Vec<(String, String)>)>, values: Vec<(u32, f64)>, ids: &mut HashMap<u32, Option<u32>>) {
        for (id, mut labels) in series {
            // The node a connection authenticated as is the node label.
            labels.retain(|(k, _)| k != "node");
            labels.push(("node".into(), node.to_string()));
            ids.insert(id, self.db.series_id(labels));
        }
        let samples: Vec<(u32, f64)> = values.into_iter().filter_map(|(id, v)| Some((ids.get(&id).copied().flatten()?, v))).collect();
        if let Err(e) = self.db.append_ids(&format!("node/{node}"), ts, &samples) {
            eprintln!("obs: cannot store samples of node {node}: {e:#}");
        }
    }

    /// A node's connection ended: its series end now.
    pub fn node_gone(&self, node: &str) {
        let _ = self.db.end_source(&format!("node/{node}"), tsdb::now_ms());
    }

    fn self_metrics(&self, api: &ApiState, node: &str) -> Vec<(Labels, f64)> {
        let parsed = super::expo::parse(&api.render_metrics());
        for (name, kind, help) in &parsed.metadata {
            self.db.set_metadata(name, kind, help);
        }
        let mut out: Vec<(Labels, f64)> = parsed
            .samples
            .into_iter()
            .map(|(mut l, v)| {
                l.push(("job".into(), "apiserver".into()));
                l.push(("instance".into(), node.to_string()));
                (l, v)
            })
            .collect();
        let job = |name: &str| -> Labels { vec![("__name__".into(), name.into()), ("job".into(), "apiserver".into()), ("instance".into(), node.to_string())] };
        out.push((job("process_resident_memory_bytes"), crate::kube::metrics::rss_bytes() as f64));
        out.push((job("process_cpu_seconds_total"), process_cpu_seconds()));
        out.push((job("up"), 1.0));
        let st = self.db.stats();
        let tsdb = |name: &str| -> Labels { vec![("__name__".into(), name.into()), ("job".into(), "litek8s".into()), ("instance".into(), node.to_string())] };
        for (name, v) in [
            ("litek8s_tsdb_series", st.series as f64),
            ("litek8s_tsdb_series_created_total", st.series_created as f64),
            ("litek8s_tsdb_series_refused_total", st.series_refused as f64),
            ("litek8s_tsdb_series_removed_total", st.series_removed as f64),
            ("litek8s_tsdb_samples_appended_total", st.samples_appended as f64),
            ("litek8s_tsdb_samples_out_of_order_total", st.samples_out_of_order as f64),
            ("litek8s_tsdb_head_bytes", st.head_bytes as f64),
            ("litek8s_tsdb_index_bytes", st.index_bytes as f64),
            ("litek8s_tsdb_disk_bytes", st.disk_bytes as f64),
            ("litek8s_tsdb_wal_bytes", st.wal_bytes as f64),
            ("litek8s_tsdb_blocks", st.blocks as f64),
        ] {
            out.push((tsdb(name), v));
        }
        out
    }

    fn alerts_series(&self) -> Vec<(Labels, f64)> {
        self.alerts
            .snapshot()
            .into_iter()
            .filter(|a| a.state != alerts::State::Resolved)
            .map(|a| {
                let mut l: Labels = a.labels.into_iter().collect();
                l.push(("__name__".into(), "ALERTS".into()));
                l.push(("alertstate".into(), if a.state == alerts::State::Firing { "firing" } else { "pending" }.into()));
                (l, 1.0)
            })
            .collect()
    }
}

fn process_cpu_seconds() -> f64 {
    // SAFETY: getrusage fills the struct we own.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0.0;
        }
        let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        t(ru.ru_utime) + t(ru.ru_stime)
    }
}

/// The CRDs of alerting: `AlertRule` (namespaced, PrometheusRule's spec),
/// `AlertReceiver` and `Silence` (cluster-wide).
pub fn crds() -> Vec<Value> {
    let crd = |plural: &str, kind: &str, scope: &str, short: &[&str], columns: Value| {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": format!("{plural}.{}", alerts::GROUP), "labels": {"app.kubernetes.io/managed-by": "litek8s"}},
            "spec": {
                "group": alerts::GROUP,
                "scope": scope,
                "names": {"plural": plural, "singular": kind.to_lowercase(), "kind": kind, "listKind": format!("{kind}List"), "shortNames": short, "categories": ["observability"]},
                "versions": [{
                    "name": "v1",
                    "served": true,
                    "storage": true,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                        "spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": true},
                    }}},
                    "additionalPrinterColumns": columns,
                }],
            },
        })
    };
    vec![
        crd("alertrules", "AlertRule", "Namespaced", &["ar"], json!([{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}])),
        crd(
            "alertreceivers",
            "AlertReceiver",
            "Cluster",
            &["arcv"],
            json!([
                {"name": "Webhook", "type": "string", "jsonPath": ".spec.webhook.url"},
                {"name": "Ntfy", "type": "string", "jsonPath": ".spec.ntfy.topic"},
                {"name": "Telegram", "type": "string", "jsonPath": ".spec.telegram.chatId"},
                {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"},
            ]),
        ),
        crd(
            "silences",
            "Silence",
            "Cluster",
            &["sil"],
            json!([
                {"name": "Ends", "type": "string", "jsonPath": ".spec.endsAt"},
                {"name": "Comment", "type": "string", "jsonPath": ".spec.comment"},
                {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"},
            ]),
        ),
    ]
}

fn install_crds(api: &ApiState) {
    for crd in crds() {
        let name = crd["metadata"]["name"].as_str().unwrap_or_default().to_string();
        if api.get_object("apiextensions.k8s.io", "customresourcedefinitions", "", &name).is_some() {
            continue;
        }
        if let Err(e) = api.create_object("apiextensions.k8s.io", "customresourcedefinitions", None, crd) {
            eprintln!("obs: cannot install CRD {name}: {}", e.message);
        }
    }
}

/// Open the store and start collecting for `api` (its HTTP endpoints then
/// answer).
pub fn start(api: &Arc<ApiState>, config: ObsConfig) -> Result<Arc<Obs>> {
    let obs = Arc::new(Obs::open(config)?);
    if api.obs.set(obs.clone()).is_err() {
        anyhow::bail!("observability already started");
    }
    tokio::spawn(run(api.clone(), obs.clone()));
    Ok(obs)
}

/// Collect, maintain and alert, until the process ends.
pub async fn run(api: Arc<ApiState>, obs: Arc<Obs>) {
    install_crds(&api);
    let node = crate::kube::agent::default_node_name();
    let interval = obs.config.interval.max(Duration::from_secs(1));
    let mut last_eval: Option<std::time::Instant> = None;
    loop {
        let now = tsdb::now_ms();
        let step = interval.as_millis() as i64;
        let next = (now / step + 1) * step;
        tokio::time::sleep(Duration::from_millis((next - now) as u64)).await;
        let (api2, obs2, node2) = (api.clone(), obs.clone(), node.clone());
        let evaluate = last_eval.is_none_or(|t| t.elapsed() >= obs.config.eval_interval);
        if evaluate {
            last_eval = Some(std::time::Instant::now());
        }
        let outgoing = tokio::task::spawn_blocking(move || {
            let (api, obs) = (api2, obs2);
            let kube = super::kube_state::collect(api.store());
            if let Err(e) = obs.db.append("kube-state", next, kube) {
                eprintln!("obs: {e:#}");
            }
            let own = obs.self_metrics(&api, &node2);
            if let Err(e) = obs.db.append("apiserver", next, own) {
                eprintln!("obs: {e:#}");
            }
            if let Err(e) = obs.db.maintain(tsdb::now_ms()) {
                eprintln!("obs: store maintenance failed: {e:#}");
            }
            if !evaluate {
                return Vec::new();
            }
            let mut rules = if obs.config.builtin_alerts { alerts::builtin_rules() } else { Vec::new() };
            rules.extend(alerts::object_rules(api.store()));
            let silences = alerts::silences(api.store());
            let now = tsdb::now_ms();
            for done in obs.alerts.evaluate(&obs.db, rules, &silences, now) {
                let _ = api.delete_named(alerts::GROUP, "silences", "", &done);
            }
            let _ = obs.db.append("alerts", next, obs.alerts_series());
            let (receivers, errors) = alerts::receivers(api.store());
            for e in errors {
                eprintln!("obs: {e}");
            }
            obs.alerts.outgoing(&receivers, now)
        })
        .await
        .unwrap_or_default();
        for o in outgoing {
            let obs = obs.clone();
            tokio::spawn(async move {
                let result = super::notify::send(&o.receiver.name, &o.receiver.target, &o.notes).await.map_err(|e| format!("{e:#}"));
                if let Err(e) = &result {
                    eprintln!("obs: notification to {} failed: {e}", o.receiver.name);
                }
                obs.alerts.sent(&o, result, tsdb::now_ms());
            });
        }
    }
}
