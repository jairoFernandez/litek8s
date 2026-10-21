//! The node agent's part of observability: sample this node every interval
//! and send the samples to the API server ([`crate::obs::node`]), keep the
//! log archive within its limits, and answer log searches
//! ([`crate::obs::logs`]).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use hyper::body::Bytes;

use super::protocol::{Frame, Message};
use super::runtime::{LogOptions, PodRuntime};
use super::{Agent, Link, finish};
use crate::obs::ObsSettings;
use crate::obs::logs::{Compiled, Follower, LogLine, LogQuery, Source, merge, normalize_ts};
use crate::obs::node::{ClaimRef, Collector, ContainerRef, PodRef, SeriesIds};

/// Every (re)connection's `Welcome` says whether to send metrics; the
/// sampler runs once per agent, following the current connection.
pub(super) fn configure<R: PodRuntime>(agent: &Arc<Agent<R>>, settings: Option<ObsSettings>) {
    let mut obs = agent.obs.lock().unwrap();
    obs.0 = settings.clone();
    match (settings, &obs.1) {
        (Some(_), None) => obs.1 = Some(tokio::spawn(sample_loop(agent.clone()))),
        (None, Some(task)) => {
            task.abort();
            obs.1 = None;
        }
        _ => {}
    }
}

fn settings<R: PodRuntime>(agent: &Agent<R>) -> Option<ObsSettings> {
    agent.obs.lock().unwrap().0.clone()
}

/// What the sampler needs to know about the pods running here.
async fn pods<R: PodRuntime>(agent: &Arc<Agent<R>>) -> Vec<PodRef> {
    let views: Vec<(String, super::PodView)> = agent.views.lock().unwrap().iter().map(|(u, v)| (u.clone(), v.clone())).collect();
    let rt = agent.runtime();
    let mut out = Vec::with_capacity(views.len());
    for (uid, v) in views {
        let mut containers = Vec::new();
        for (name, (id, _)) in &v.containers {
            let Some(id) = id else { continue };
            let cgroup = rt.container_cgroup(id);
            let runtime_stats = if cgroup.is_none() { rt.stats(id).await.ok().flatten() } else { None };
            containers.push(ContainerRef { name: name.clone(), image: v.images.get(name).cloned().unwrap_or_default(), cgroup, runtime_stats });
        }
        let host_if = v.sandbox.as_deref().and_then(|s| rt.sandbox_netdev(s));
        out.push(PodRef { namespace: v.namespace, name: v.name, uid, containers, host_if });
    }
    out
}

async fn sample_loop<R: PodRuntime>(agent: Arc<Agent<R>>) {
    let root = agent.config().root_dir.clone();
    let node = agent.config().node_name.clone();
    let mut collector = Some(Collector::new(&node, &root));
    let mut ids = SeriesIds::default();
    let mut last_link: Option<Arc<Link>> = None;
    let mut last_sweep: Option<std::time::Instant> = None;
    loop {
        let Some(s) = settings(&agent) else { return };
        let interval = Duration::from_millis(s.interval_ms.max(1000));
        // Sample on the interval's grid, so timestamps are regular (they
        // then compress to a bit each).
        let now = crate::obs::tsdb::now_ms();
        let next = (now / interval.as_millis() as i64 + 1) * interval.as_millis() as i64;
        tokio::time::sleep(Duration::from_millis((next - now) as u64)).await;
        let Some(link) = agent.link() else { continue };
        if last_link.as_ref().is_none_or(|l| !Arc::ptr_eq(l, &link)) {
            ids.reset_sent();
            last_link = Some(link.clone());
        }
        let pods = pods(&agent).await;
        let claims: Vec<ClaimRef> = agent.claims.lock().unwrap().values().flatten().cloned().collect();
        let Some(mut c) = collector.take() else { return };
        let Ok((c, samples)) = tokio::task::spawn_blocking(move || {
            let samples = c.sample(&pods, &claims);
            (c, samples)
        })
        .await
        else {
            return;
        };
        collector = Some(c);
        let mut samples = samples;
        if let Some(overlay) = &agent.overlay {
            samples.extend(overlay.metrics(&node));
        }
        let (series, values) = ids.encode(samples);
        let _ = link.send(Message::Metrics { ts: next, series, values }).await;

        if last_sweep.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
            last_sweep = Some(std::time::Instant::now());
            let archive = root.join("logs");
            let (retention, max) = (Duration::from_secs(s.log_retention_secs), s.log_max_bytes);
            let _ = tokio::task::spawn_blocking(move || crate::obs::logs::sweep(&archive, retention, max)).await;
        }
    }
}

/// A container the query wants, and where its logs are.
struct Live {
    source: Source,
    /// The runtime keeps no files we can read (Docker): its log API.
    runtime_id: Option<String>,
}

fn live_sources<R: PodRuntime>(agent: &Agent<R>, q: &Compiled) -> Vec<Live> {
    let views: Vec<super::PodView> = agent.views.lock().unwrap().values().cloned().collect();
    let rt = agent.runtime();
    let mut out = Vec::new();
    for v in views {
        for (container, (current, previous)) in &v.containers {
            if !q.wants(&v.namespace, &v.name, container, &v.labels) {
                continue;
            }
            for (id, live) in [(previous, false), (current, true)] {
                let Some(id) = id else { continue };
                let files = rt.log_files(id);
                let source = Source { namespace: v.namespace.clone(), pod: v.name.clone(), container: container.clone(), labels: v.labels.clone(), files: files.clone(), live };
                out.push(Live { source, runtime_id: files.is_empty().then(|| id.clone()) });
            }
        }
    }
    out
}

/// Lines from a runtime's log API (`<RFC 3339 time> <text>` with
/// timestamps on).
async fn runtime_lines<R: PodRuntime>(agent: &Agent<R>, id: &str, src: &Source, q: &Compiled, node: &str) -> VecDeque<LogLine> {
    let since = q.q.since_ms.map(|ms| ((crate::obs::tsdb::now_ms() - ms) / 1000).max(1));
    let mut logs = agent.runtime().logs(id, LogOptions { follow: false, tail_lines: None, timestamps: true, since_seconds: since });
    let mut out = VecDeque::new();
    let mut pending: Vec<u8> = Vec::new();
    while let Some(Ok((channel, bytes))) = logs.next().await {
        pending.extend_from_slice(&bytes);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            let Some((ts, rest)) = text.split_once(' ') else { continue };
            let Some(ts) = normalize_ts(ts) else { continue };
            if !q.wants_line(&ts, rest) {
                continue;
            }
            if out.len() == q.limit {
                out.pop_front();
            }
            out.push_back(LogLine {
                ts,
                namespace: src.namespace.clone(),
                pod: src.pod.clone(),
                container: src.container.clone(),
                node: node.to_string(),
                stream: if channel == 2 { "stderr" } else { "stdout" }.into(),
                line: rest.to_string(),
            });
        }
    }
    out
}

async fn send_lines(link: &Link, stream: u32, lines: &[LogLine]) -> bool {
    let mut buf = Vec::new();
    for l in lines {
        if serde_json::to_writer(&mut buf, l).is_err() {
            continue;
        }
        buf.push(b'\n');
        if buf.len() > 64 * 1024 {
            if link.tx.send(Frame::Data { stream, channel: 1, bytes: Bytes::from(std::mem::take(&mut buf)) }).await.is_err() {
                return false;
            }
        }
    }
    buf.is_empty() || link.tx.send(Frame::Data { stream, channel: 1, bytes: Bytes::from(buf) }).await.is_ok()
}

pub(super) async fn log_query_stream<R: PodRuntime>(agent: Arc<Agent<R>>, link: Arc<Link>, stream: u32, query: LogQuery) {
    let q = match Compiled::new(query) {
        Ok(q) => Arc::new(q),
        Err(e) => return finish(&link, stream, None, Some(format!("{e:#}"))).await,
    };
    let node = agent.config().node_name.clone();
    let archive = agent.config().root_dir.join("logs");
    let live = live_sources(&agent, &q);
    let (files, runtime): (Vec<Live>, Vec<Live>) = live.into_iter().partition(|l| l.runtime_id.is_none());
    let file_sources: Vec<Source> = files.into_iter().map(|l| l.source).collect();
    let history = {
        let (q, node, sources) = (q.clone(), node.clone(), file_sources.clone());
        tokio::task::spawn_blocking(move || {
            let mut parts: Vec<VecDeque<LogLine>> = sources.iter().map(|s| s.search(&q, &node)).collect();
            if !q.q.live_only {
                for s in crate::obs::logs::archived(&archive) {
                    if q.wants(&s.namespace, &s.pod, &s.container, &s.labels) {
                        parts.push(s.search(&q, &node));
                    }
                }
            }
            parts
        })
        .await
        .unwrap_or_default()
    };
    let mut parts = history;
    for r in &runtime {
        if let Some(id) = &r.runtime_id {
            parts.push(runtime_lines(&agent, id, &r.source, &q, &node).await);
        }
    }
    let merged = merge(parts, q.limit);
    if !send_lines(&link, stream, &merged).await {
        return;
    }
    if !q.q.follow {
        return finish(&link, stream, None, None).await;
    }
    // Follow: tail files; runtime-only containers through their log API.
    let mut follower = Follower::default();
    follower.start(&file_sources);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogLine>(1024);
    let mut followed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tasks = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let live = live_sources(&agent, &q);
                let mut sources = Vec::new();
                for l in live {
                    match l.runtime_id {
                        None => sources.push(l.source),
                        Some(id) if l.source.live && followed.insert(id.clone()) => {
                            tasks.push(tokio::spawn(follow_runtime(agent.clone(), id, l.source, q.clone(), node.clone(), tx.clone())));
                        }
                        Some(_) => {}
                    }
                }
                let (q2, node2) = (q.clone(), node.clone());
                let Ok((f, lines)) = tokio::task::spawn_blocking(move || {
                    let lines = follower.poll(&sources, &q2, &node2);
                    (follower, lines)
                }).await else { break };
                follower = f;
                if !send_lines(&link, stream, &lines).await {
                    break;
                }
            }
            Some(line) = rx.recv() => {
                if !send_lines(&link, stream, &[line]).await {
                    break;
                }
            }
        }
    }
    for t in tasks {
        t.abort();
    }
}

async fn follow_runtime<R: PodRuntime>(agent: Arc<Agent<R>>, id: String, src: Source, q: Arc<Compiled>, node: String, tx: tokio::sync::mpsc::Sender<LogLine>) {
    let mut logs = agent.runtime().logs(&id, LogOptions { follow: true, tail_lines: Some(0), timestamps: true, since_seconds: None });
    let mut pending: Vec<u8> = Vec::new();
    while let Some(Ok((channel, bytes))) = logs.next().await {
        pending.extend_from_slice(&bytes);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            let Some((ts, rest)) = text.split_once(' ') else { continue };
            let Some(ts) = normalize_ts(ts) else { continue };
            if !q.grep_matches(rest) {
                continue;
            }
            let l = LogLine {
                ts,
                namespace: src.namespace.clone(),
                pod: src.pod.clone(),
                container: src.container.clone(),
                node: node.clone(),
                stream: if channel == 2 { "stderr" } else { "stdout" }.into(),
                line: rest.to_string(),
            };
            if tx.send(l).await.is_err() {
                return;
            }
        }
    }
}
