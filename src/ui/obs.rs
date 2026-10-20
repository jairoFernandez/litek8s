//! The Observe view's backend: pod CPU and memory over the last half hour
//! and the cluster's alerts, from the API server's built-in metrics
//! (`/prometheus/api/v1/query_range`, `/litek8s/v1/alerts`), sampled every
//! 10 s while the view is on screen and not at all otherwise; cluster-wide
//! log search (`/litek8s/v1/logs`) into the log view; acknowledging alerts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use hyper::Method;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::{AlertRow, Msg, ObsSample, PodUsage};
use super::client::{Endpoint, Lines, enc};

/// How often the view refreshes while shown.
pub const EVERY: Duration = Duration::from_secs(10);
/// The sparklines' window and points.
const WINDOW_SECS: i64 = 30 * 60;
const POINTS: i64 = 16;

pub type Shared = Arc<Mutex<Option<Endpoint>>>;

pub fn spawn(tx: UnboundedSender<Msg>, visible: Arc<AtomicBool>, wake: Arc<Notify>, ep: Shared) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if !visible.load(Ordering::Relaxed) {
                wake.notified().await;
                continue;
            }
            let endpoint = ep.lock().unwrap().clone();
            let sample = match endpoint {
                Some(ep) => sample(&ep).await.unwrap_or_else(|e| ObsSample { error: Some(explain(format!("{e:#}"))), ..Default::default() }),
                None => ObsSample { error: Some("the cluster is not running".into()), ..Default::default() },
            };
            if tx.send(Msg::Obs(Box::new(sample))).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(EVERY) => {}
                _ = wake.notified() => {}
            }
        }
    })
}

/// A server without the Prometheus API answers 404: one started before
/// observability existed (a dev VM still on an older guest).
fn explain(e: String) -> String {
    if e.contains("could not find the requested resource") {
        "this cluster's API server has no metrics API: it predates observability; restart the cluster on the new binary (litek8s dev down --all && litek8s dev up)".into()
    } else {
        e
    }
}

async fn range(ep: &Endpoint, query: &str, now: i64) -> Result<Vec<(BTreeMap<String, String>, Vec<(i64, f64)>)>> {
    let step = WINDOW_SECS / POINTS;
    let path = format!("/prometheus/api/v1/query_range?query={}&start={}&end={now}&step={step}", enc(query), now - WINDOW_SECS);
    let v = ep.json(Method::GET, &path, None).await?;
    if v["status"] != "success" {
        bail!("{}", v["error"].as_str().unwrap_or("query failed"));
    }
    Ok(v["data"]["result"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let labels = r["metric"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default();
            let points = r["values"].as_array().into_iter().flatten().filter_map(|p| Some((p[0].as_f64()? as i64, p[1].as_str()?.parse().ok()?))).collect();
            (labels, points)
        })
        .collect())
}

/// Values at each step of the window (`None`: no sample then).
fn spread(points: &[(i64, f64)], now: i64) -> Vec<Option<f64>> {
    let step = WINDOW_SECS / POINTS;
    let start = now - WINDOW_SECS;
    let mut out = vec![None; POINTS as usize + 1];
    for (t, v) in points {
        let i = ((t - start) / step) as usize;
        if let Some(slot) = out.get_mut(i) {
            *slot = Some(*v);
        }
    }
    out
}

pub async fn sample(ep: &Endpoint) -> Result<ObsSample> {
    let now = chrono::Utc::now().timestamp();
    let cpu = range(ep, r#"sum by (namespace, pod) (rate(container_cpu_usage_seconds_total{container!=""}[2m]))"#, now).await?;
    let mem = range(ep, r#"sum by (namespace, pod) (container_memory_working_set_bytes{container!=""})"#, now).await?;
    let mut pods: BTreeMap<(String, String), PodUsage> = BTreeMap::new();
    for (series, is_mem) in [(cpu, false), (mem, true)] {
        for (labels, points) in series {
            let key = (labels.get("namespace").cloned().unwrap_or_default(), labels.get("pod").cloned().unwrap_or_default());
            let p = pods.entry(key.clone()).or_insert_with(|| PodUsage { namespace: key.0, name: key.1, ..Default::default() });
            let values = spread(&points, now);
            if is_mem {
                p.mem = values;
            } else {
                p.cpu = values;
            }
        }
    }
    let a = ep.json(Method::GET, "/litek8s/v1/alerts", None).await?;
    let mut alerts: Vec<AlertRow> = a["alerts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["state"] != "resolved")
        .map(|r| {
            let l = &r["labels"];
            let s = |k: &str| l[k].as_str().unwrap_or_default().to_string();
            AlertRow {
                state: r["state"].as_str().unwrap_or_default().to_string(),
                name: s("alertname"),
                target: ["namespace", "pod", "container", "node", "persistentvolumeclaim"].iter().filter_map(|k| l[*k].as_str()).collect::<Vec<_>>().join("/"),
                severity: s("severity"),
                since: r["active_at"].as_i64().unwrap_or_default() / 1000,
                summary: r["annotations"]["summary"].as_str().unwrap_or_default().to_string(),
                silenced: r["silenced_by"].as_array().is_some_and(|s| !s.is_empty()),
                fingerprint: r["fingerprint"].as_str().unwrap_or_default().to_string(),
                pod: l["pod"].as_str().map(|p| (s("namespace"), p.to_string())),
                labels: l.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default(),
            }
        })
        .collect();
    alerts.sort_by_key(|a| (a.state != "firing", a.silenced, a.severity != "critical", a.since));
    Ok(ObsSample { pods: pods.into_values().collect(), alerts, error: None, at: now })
}

/// A cluster-wide log search into the log view (following).
pub async fn search(ep: Endpoint, namespace: Option<String>, pod: Option<String>, grep: String, id: u64, tx: UnboundedSender<Msg>) {
    let mut q = vec!["since=1h".to_string(), "follow=true".to_string(), "limit=2000".to_string()];
    if !grep.is_empty() {
        q.push(format!("grep={}", enc(&grep)));
    }
    if let Some(n) = namespace {
        q.push(format!("namespace={}", enc(&n)));
    }
    if let Some(p) = pod {
        q.push(format!("pod={}", enc(&format!("^{}$", regex::escape(&p)))));
    }
    let resp = match ep.send(Method::GET, &format!("/litek8s/v1/logs?{}", q.join("&")), None, Vec::new()).await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let _ = tx.send(Msg::LogEnd(id, Some(format!("log search failed: {}", r.status()))));
            return;
        }
        Err(e) => {
            let _ = tx.send(Msg::LogEnd(id, Some(format!("{e:#}"))));
            return;
        }
    };
    let mut lines = Lines::new(resp.into_body());
    let mut batch = Vec::new();
    loop {
        let next = if batch.is_empty() {
            lines.next().await
        } else {
            // Send what came in a burst together.
            match tokio::time::timeout(Duration::from_millis(50), lines.next()).await {
                Ok(l) => l,
                Err(_) => {
                    let _ = tx.send(Msg::LogLines(id, std::mem::take(&mut batch)));
                    continue;
                }
            }
        };
        let Some(line) = next else { break };
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        if let Some(e) = v["error"].as_str() {
            batch.push(format!("! node {}: {e}", s("node")));
            continue;
        }
        let ts = chrono::DateTime::parse_from_rfc3339(&s("ts")).map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
        batch.push(format!("{ts} {}/{} {}│ {}", s("namespace"), s("pod"), s("container"), s("line")));
        if batch.len() >= 200 {
            let _ = tx.send(Msg::LogLines(id, std::mem::take(&mut batch)));
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(Msg::LogLines(id, batch));
    }
    let _ = tx.send(Msg::LogEnd(id, None));
}

/// Acknowledge alerts: a silence per alert on all its labels, removed by
/// the server when the alert resolves.
pub async fn ack(ep: &Endpoint, alerts: &[AlertRow]) -> Result<String> {
    for a in alerts {
        let matchers: Vec<Value> = a.labels.iter().map(|(k, v)| json!({"name": k, "value": v, "isRegex": false, "isEqual": true})).collect();
        let ends = chrono::Utc::now() + chrono::Duration::days(7);
        let obj = json!({
            "apiVersion": "observability.litek8s.dev/v1",
            "kind": "Silence",
            "metadata": {"generateName": "ack-", "annotations": {crate::obs::alerts::UNTIL_RESOLVED: "true"}},
            "spec": {"matchers": matchers, "endsAt": ends.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "comment": "acknowledged in litek8s ui", "createdBy": std::env::var("USER").unwrap_or_default()},
        });
        ep.json(Method::POST, "/apis/observability.litek8s.dev/v1/silences", Some(&obj)).await?;
    }
    Ok(format!("acknowledged {} alert(s): silenced until resolved", alerts.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_points_over_the_window() {
        let now = 10_000;
        let step = WINDOW_SECS / POINTS;
        let s = spread(&[(now - WINDOW_SECS, 1.0), (now - step, 2.0), (now, 3.0)], now);
        assert_eq!(s.len(), POINTS as usize + 1);
        assert_eq!((s[0], s[POINTS as usize - 1], s[POINTS as usize]), (Some(1.0), Some(2.0), Some(3.0)));
        assert_eq!(s[1], None);
    }
}
