//! HTTP endpoints:
//!
//! - the Prometheus HTTP API under `/prometheus` (`/api/v1/query`,
//!   `query_range`, `series`, `labels`, `label/<name>/values`, `metadata`,
//!   `rules`, `alerts`, `status/buildinfo`) and `/prometheus/federate`, so
//!   Grafana can use `http://<server>:6443/prometheus` as a Prometheus data
//!   source (also at the root of `--prometheus-listen` when given);
//! - `GET /litek8s/v1/logs`: log search across nodes, as JSON lines;
//! - `GET /litek8s/v1/alerts`: alerts with their silences, rule and
//!   receiver errors; `GET /litek8s/v1/observability`: the store's state.

use std::sync::Arc;

use hyper::body::{Bytes, Frame};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{Method, Response, StatusCode};
use serde_json::{Value, json};

use super::logs::{LogLine, LogQuery};
use super::promql::{self, Value as QValue, format_value};
use super::server::Obs;
use super::tsdb::{self, Labels, Matcher};
use crate::kube::agent::hub::StreamEvent;
use crate::kube::agent::protocol::Message;
use crate::kube::server::{ApiState, Body, full};

/// Query parameters (repeated keys kept), from the URL and a form body.
pub struct Params(Vec<(String, String)>);

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Params {
    pub fn parse(query: &str, body: &[u8], content_type: &str) -> Self {
        let mut v = Vec::new();
        let mut add = |s: &str| {
            for pair in s.split('&').filter(|p| !p.is_empty()) {
                let (k, val) = pair.split_once('=').unwrap_or((pair, ""));
                v.push((percent_decode(k), percent_decode(val)));
            }
        };
        add(query);
        if content_type.starts_with("application/x-www-form-urlencoded") {
            add(&String::from_utf8_lossy(body));
        }
        Params(v)
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(key, v)| key == k && !v.is_empty()).map(|(_, v)| v.as_str())
    }

    fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(key, _)| key == k).map(|(_, v)| v.as_str()).collect()
    }
}

fn respond(code: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(full(body));
    *r.status_mut() = code;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

fn success(data: Value) -> Response<Body> {
    respond(StatusCode::OK, "application/json", serde_json::to_vec(&json!({"status": "success", "data": data})).unwrap_or_default())
}

fn error(code: StatusCode, kind: &str, msg: impl std::fmt::Display) -> Response<Body> {
    respond(code, "application/json", serde_json::to_vec(&json!({"status": "error", "errorType": kind, "error": msg.to_string()})).unwrap_or_default())
}

/// A time parameter: unix seconds (fractional) or RFC 3339.
fn time_param(p: &Params, key: &str, default: i64) -> Result<i64, String> {
    match p.get(key) {
        None => Ok(default),
        Some(s) => {
            if let Ok(secs) = s.parse::<f64>() {
                return Ok((secs * 1000.0).round() as i64);
            }
            chrono::DateTime::parse_from_rfc3339(s).map(|t| t.timestamp_millis()).map_err(|_| format!("invalid parameter {key:?}: cannot parse {s:?} to a valid timestamp"))
        }
    }
}

fn duration_param(s: &str) -> Result<i64, String> {
    if let Ok(secs) = s.parse::<f64>() {
        return Ok((secs * 1000.0).round() as i64);
    }
    tsdb::parse_duration(s).map(|d| d.as_millis() as i64).map_err(|_| format!("cannot parse {s:?} to a valid duration"))
}

fn ts(ms: i64) -> Value {
    json!(ms as f64 / 1000.0)
}

fn metric(labels: &Labels) -> Value {
    Value::Object(labels.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

fn selectors(p: &Params) -> Result<Vec<Vec<Matcher>>, String> {
    p.all("match[]")
        .into_iter()
        .map(|m| match promql::parse(m).map_err(|e| e.to_string())? {
            promql::Expr::Selector(s) => Ok(s.all_matchers()),
            _ => Err(format!("match[] must be a series selector: {m}")),
        })
        .collect()
}

/// Everything after `/prometheus` (or the root of the Prometheus port).
pub fn prometheus(obs: &Obs, method: &Method, path: &str, p: &Params) -> Response<Body> {
    if method != Method::GET && method != Method::POST {
        return error(StatusCode::METHOD_NOT_ALLOWED, "bad_data", "method not allowed");
    }
    let now = tsdb::now_ms();
    let oldest = now - obs.config.tiers.iter().map(|t| t.retention.as_millis() as i64).max().unwrap_or(0);
    let path = path.trim_end_matches('/');
    match path {
        "/api/v1/query" => {
            let Some(q) = p.get("query") else { return error(StatusCode::BAD_REQUEST, "bad_data", "invalid parameter \"query\": empty query") };
            let t = match time_param(p, "time", now) {
                Ok(t) => t,
                Err(e) => return error(StatusCode::BAD_REQUEST, "bad_data", e),
            };
            match promql::instant(&obs.db, q, t) {
                Ok(QValue::Scalar(v)) => success(json!({"resultType": "scalar", "result": [ts(t), format_value(v)]})),
                Ok(QValue::Str(s)) => success(json!({"resultType": "string", "result": [ts(t), s]})),
                Ok(QValue::Vector(v)) => success(json!({
                    "resultType": "vector",
                    "result": v.iter().map(|s| json!({"metric": metric(&s.labels), "value": [ts(t), format_value(s.v)]})).collect::<Vec<_>>(),
                })),
                Ok(QValue::Matrix(m)) => success(json!({
                    "resultType": "matrix",
                    "result": m.iter().map(|s| json!({"metric": metric(&s.labels), "values": s.points.iter().map(|(t, v)| json!([ts(*t), format_value(*v)])).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                })),
                Err(e) => error(StatusCode::BAD_REQUEST, "bad_data", format!("{e:#}")),
            }
        }
        "/api/v1/query_range" => {
            let Some(q) = p.get("query") else { return error(StatusCode::BAD_REQUEST, "bad_data", "invalid parameter \"query\": empty query") };
            let (start, end) = match (time_param(p, "start", now - 3_600_000), time_param(p, "end", now)) {
                (Ok(s), Ok(e)) => (s, e),
                (Err(e), _) | (_, Err(e)) => return error(StatusCode::BAD_REQUEST, "bad_data", e),
            };
            let step = match p.get("step").map(duration_param).unwrap_or(Ok(15_000)) {
                Ok(s) => s,
                Err(e) => return error(StatusCode::BAD_REQUEST, "bad_data", format!("invalid parameter \"step\": {e}")),
            };
            match promql::range(&obs.db, q, start, end, step) {
                Ok(series) => success(json!({
                    "resultType": "matrix",
                    "result": series
                        .iter()
                        .map(|s| json!({"metric": metric(&s.labels), "values": s.points.iter().map(|(t, v)| json!([ts(*t), format_value(*v)])).collect::<Vec<_>>()}))
                        .collect::<Vec<_>>(),
                })),
                Err(e) => error(StatusCode::BAD_REQUEST, "bad_data", format!("{e:#}")),
            }
        }
        "/api/v1/series" | "/api/v1/labels" => {
            let sel = match selectors(p) {
                Ok(s) => s,
                Err(e) => return error(StatusCode::BAD_REQUEST, "bad_data", e),
            };
            let (start, end) = (time_param(p, "start", oldest).unwrap_or(oldest), time_param(p, "end", now).unwrap_or(now));
            if path.ends_with("series") {
                if sel.is_empty() {
                    return error(StatusCode::BAD_REQUEST, "bad_data", "no match[] parameter provided");
                }
                success(json!(obs.db.series(&sel, start, end).iter().map(metric).collect::<Vec<_>>()))
            } else {
                success(json!(obs.db.label_names(&sel, start, end)))
            }
        }
        "/api/v1/metadata" => {
            let only = p.get("metric");
            let mut out = serde_json::Map::new();
            for (name, (kind, help)) in obs.db.metadata() {
                if only.is_none_or(|m| m == name) {
                    out.insert(name, json!([{"type": kind, "help": help, "unit": ""}]));
                }
            }
            success(Value::Object(out))
        }
        "/api/v1/status/buildinfo" => success(json!({
            "version": "2.53.0",
            "revision": "litek8s",
            "branch": "litek8s",
            "buildUser": "litek8s",
            "buildDate": "",
            "goVersion": "",
        })),
        "/api/v1/status/runtimeinfo" => success(json!({"startTime": "", "CWD": "", "reloadConfigSuccess": true, "storageRetention": "", "timeSeriesCount": obs.db.stats().series})),
        "/api/v1/status/tsdb" => success(json!({"headStats": {"numSeries": obs.db.stats().series}, "seriesCountByMetricName": [], "labelValueCountByLabelName": [], "memoryInBytesByLabelName": [], "seriesCountByLabelValuePair": []})),
        "/api/v1/query_exemplars" => success(json!([])),
        "/api/v1/targets" => success(json!({"activeTargets": [], "droppedTargets": []})),
        "/api/v1/rules" => {
            let snapshot = obs.alerts.snapshot();
            let errors = obs.alerts.errors();
            let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
            for r in obs.alerts.rules() {
                let alerts: Vec<Value> = snapshot.iter().filter(|a| a.labels.get("alertname") == Some(&r.name) && a.state != super::alerts::State::Resolved).map(alert_json).collect();
                let state = if alerts.iter().any(|a| a["state"] == "firing") {
                    "firing"
                } else if alerts.is_empty() {
                    "inactive"
                } else {
                    "pending"
                };
                let err = errors.get(&format!("{}/{}", r.source, r.name));
                let rule = json!({
                    "name": r.name, "query": r.expr, "duration": r.for_ms / 1000, "labels": r.labels, "annotations": r.annotations,
                    "alerts": alerts, "health": if err.is_some() { "err" } else { "ok" }, "lastError": err.cloned().unwrap_or_default(),
                    "type": "alerting", "state": state, "evaluationTime": 0, "lastEvaluation": "",
                });
                match groups.iter_mut().find(|g| g.0 == r.source) {
                    Some(g) => g.1.push(rule),
                    None => groups.push((r.source.clone(), vec![rule])),
                }
            }
            success(json!({"groups": groups.into_iter().map(|(name, rules)| json!({"name": name, "file": name, "rules": rules, "interval": obs.config.eval_interval.as_secs()})).collect::<Vec<_>>()}))
        }
        "/api/v1/alerts" => success(json!({"alerts": obs.alerts.snapshot().iter().filter(|a| a.state != super::alerts::State::Resolved).map(alert_json).collect::<Vec<_>>()})),
        "/federate" => {
            let sel = match selectors(p) {
                Ok(s) => s,
                Err(e) => return error(StatusCode::BAD_REQUEST, "bad_data", e),
            };
            let mut out = String::new();
            for m in &sel {
                let Ok(series) = obs.db.select(m, now - promql::eval::LOOKBACK_MS, now, 1) else { continue };
                for s in series {
                    if let Some((t, v)) = s.samples.last().copied().filter(|(_, v)| !tsdb::is_stale(*v)) {
                        super::expo::write_sample(&mut out, &s.labels, v, Some(t));
                    }
                }
            }
            respond(StatusCode::OK, "text/plain; version=0.0.4", out)
        }
        label if label.starts_with("/api/v1/label/") && label.ends_with("/values") => {
            let name = &label["/api/v1/label/".len()..label.len() - "/values".len()];
            let sel = match selectors(p) {
                Ok(s) => s,
                Err(e) => return error(StatusCode::BAD_REQUEST, "bad_data", e),
            };
            let (start, end) = (time_param(p, "start", oldest).unwrap_or(oldest), time_param(p, "end", now).unwrap_or(now));
            success(json!(obs.db.label_values(&percent_decode(name), &sel, start, end)))
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", format!("unknown endpoint {path}")),
    }
}

fn alert_json(a: &super::alerts::Alert) -> Value {
    json!({
        "labels": a.labels,
        "annotations": a.annotations,
        "state": a.state,
        "activeAt": chrono::DateTime::from_timestamp_millis(a.active_at).unwrap_or_default().to_rfc3339(),
        "value": format_value(a.value),
    })
}

/// `GET /litek8s/v1/alerts`
pub fn alerts(obs: &Obs) -> Response<Body> {
    let body = json!({
        "alerts": obs.alerts.snapshot(),
        "ruleErrors": obs.alerts.errors(),
        "receiverErrors": obs.alerts.receiver_errors(),
        "rules": obs.alerts.rules().len(),
    });
    respond(StatusCode::OK, "application/json", serde_json::to_vec(&body).unwrap_or_default())
}

/// `GET /litek8s/v1/observability`
pub fn status(obs: &Obs) -> Response<Body> {
    let st = obs.db.stats();
    let body = json!({
        "store": st,
        "memoryBytes": st.head_bytes + st.index_bytes,
        "memoryBudget": obs.config.memory_budget,
        "intervalSeconds": obs.config.interval.as_secs(),
        "dir": obs.db.dir().display().to_string(),
    });
    respond(StatusCode::OK, "application/json", serde_json::to_vec(&body).unwrap_or_default())
}

/// `GET /litek8s/v1/logs`: `namespace`, `selector`, `pod` (regex),
/// `container`, `since` (`1h` or RFC 3339), `until`, `grep` (regex),
/// `limit`, `follow`, `node`. JSON lines of [`LogLine`]; a node that
/// cannot answer adds `{"node": ..., "error": ...}`.
pub async fn logs(api: &Arc<ApiState>, p: &Params) -> Response<Body> {
    let now = tsdb::now_ms();
    let time = |k: &str| p.get(k).map(|s| super::logs::parse_time(s, now)).transpose();
    let (since, until) = match (time("since"), time("until")) {
        (Ok(s), Ok(u)) => (s, u),
        (Err(e), _) | (_, Err(e)) => return error(StatusCode::BAD_REQUEST, "bad_data", format!("{e:#}")),
    };
    let follow = matches!(p.get("follow"), Some("true" | "1"));
    let query = LogQuery {
        namespace: p.get("namespace").map(str::to_string),
        selector: p.get("selector").or(p.get("labelSelector")).map(str::to_string),
        pod: p.get("pod").map(str::to_string),
        container: p.get("container").map(str::to_string),
        since_ms: since,
        until_ms: until,
        grep: p.get("grep").map(str::to_string),
        limit: p.get("limit").and_then(|l| l.parse().ok()),
        follow,
        live_only: matches!(p.get("liveOnly"), Some("true" | "1")),
    };
    if let Err(e) = super::logs::Compiled::new(query.clone()) {
        return error(StatusCode::BAD_REQUEST, "bad_data", format!("{e:#}"));
    }
    let mut nodes = api.hub.nodes_with_feature("logs");
    if let Some(n) = p.get("node") {
        nodes.retain(|x| x == n);
    }
    let limit = query.limit.unwrap_or(super::logs::DEFAULT_LIMIT).clamp(1, super::logs::MAX_LIMIT);
    let mut handles = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    for node in nodes {
        let q = query.clone();
        match api.hub.open_stream(&node, move |stream| Message::LogQuery { stream, query: q }).await {
            Ok(h) => handles.push((node, h)),
            Err(e) => errors.push((node, format!("{e:#}"))),
        }
    }
    if follow {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(256);
        for (node, mut h) in handles {
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = h.events.recv().await {
                    let bytes = match ev {
                        StreamEvent::Data { bytes, .. } => bytes,
                        StreamEvent::Done { error: Some(e), .. } => Bytes::from(format!("{}\n", json!({"node": node, "error": e}))),
                        StreamEvent::Done { .. } => break,
                    };
                    if tx.send(bytes).await.is_err() {
                        break;
                    }
                }
            });
        }
        for (node, e) in errors {
            let _ = tx.send(Bytes::from(format!("{}\n", json!({"node": node, "error": e})))).await;
        }
        drop(tx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(Frame::data(b)), rx)) });
        let mut r = Response::new(http_body_util::BodyExt::boxed_unsync(http_body_util::StreamBody::new(stream)));
        r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
        return r;
    }
    let results = futures_util::future::join_all(handles.into_iter().map(|(node, mut h)| async move {
        let mut buf = Vec::new();
        let err;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(60));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                ev = h.events.recv() => match ev {
                    Some(StreamEvent::Data { bytes, .. }) => buf.extend_from_slice(&bytes),
                    Some(StreamEvent::Done { error, .. }) => {
                        err = error;
                        break;
                    }
                    None => {
                        err = Some("node disconnected".to_string());
                        break;
                    }
                },
                _ = &mut deadline => {
                    err = Some("timed out".to_string());
                    break;
                }
            }
        }
        (node, buf, err)
    }))
    .await;
    let mut parts = Vec::new();
    for (node, buf, err) in results {
        let lines: std::collections::VecDeque<LogLine> = buf.split(|b| *b == b'\n').filter(|l| !l.is_empty()).filter_map(|l| serde_json::from_slice(l).ok()).collect();
        parts.push(lines);
        if let Some(e) = err {
            errors.push((node, e));
        }
    }
    let merged = super::logs::merge(parts, limit);
    let mut out = Vec::with_capacity(merged.len() * 160);
    for l in &merged {
        let _ = serde_json::to_writer(&mut out, l);
        out.push(b'\n');
    }
    for (node, e) in errors {
        let _ = serde_json::to_writer(&mut out, &json!({"node": node, "error": e}));
        out.push(b'\n');
    }
    respond(StatusCode::OK, "application/x-ndjson", out)
}

/// Serve the Prometheus API at the root of its own port
/// (`--prometheus-listen`), for tools that expect `:9090`.
pub async fn serve_prometheus(obs: Arc<Obs>, listener: tokio::net::TcpListener) {
    use http_body_util::BodyExt;
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let obs = obs.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let obs = obs.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = body.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
                    let ct = parts.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                    let p = Params::parse(parts.uri.query().unwrap_or_default(), &body, &ct);
                    let path = parts.uri.path();
                    let resp = match path {
                        "/-/healthy" | "/-/ready" => respond(StatusCode::OK, "text/plain", "Prometheus Server is Ready.\n"),
                        _ => {
                            let o = obs.clone();
                            let (m, path) = (parts.method.clone(), path.to_string());
                            tokio::task::spawn_blocking(move || prometheus(&o, &m, &path, &p)).await.unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e))
                        }
                    };
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
        });
    }
}

/// Requests the API server routes here (`None`: not ours).
pub async fn route(api: &Arc<ApiState>, method: &Method, path: &str, query: &str, body: &[u8], content_type: &str) -> Option<Response<Body>> {
    let ours = path.starts_with("/prometheus/") || path == "/prometheus" || path.starts_with("/litek8s/v1/logs") || path == "/litek8s/v1/alerts" || path == "/litek8s/v1/observability";
    if !ours {
        return None;
    }
    let Some(obs) = api.obs.get().cloned() else {
        return Some(error(StatusCode::NOT_FOUND, "not_found", "observability is off on this server (litek8s apiserver --no-observability)"));
    };
    let p = Params::parse(query, body, content_type);
    Some(match path {
        "/litek8s/v1/logs" => logs(api, &p).await,
        "/litek8s/v1/alerts" => alerts(&obs),
        "/litek8s/v1/observability" => status(&obs),
        _ => {
            let rest = path.strip_prefix("/prometheus").unwrap_or(path).to_string();
            let method = method.clone();
            if rest == "/-/healthy" || rest == "/-/ready" {
                return Some(respond(StatusCode::OK, "text/plain", "Prometheus Server is Ready.\n"));
            }
            tokio::task::spawn_blocking(move || prometheus(&obs, &method, &rest, &p)).await.unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_from_query_and_form() {
        let p = Params::parse("query=up%7Bjob%3D%22x%22%7D&match[]=a&match[]=b", b"start=1&step=15s", "application/x-www-form-urlencoded");
        assert_eq!(p.get("query"), Some("up{job=\"x\"}"));
        assert_eq!(p.all("match[]"), vec!["a", "b"]);
        assert_eq!(p.get("step"), Some("15s"));
        assert_eq!(percent_decode("a+b%20c%"), "a b c%");
        assert_eq!(time_param(&p, "start", 0), Ok(1000));
        assert_eq!(duration_param("1m"), Ok(60_000));
    }
}
