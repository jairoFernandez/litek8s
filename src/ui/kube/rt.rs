//! The Kubernetes view's runtime: informers started and stopped as views
//! come and go (the last few unused ones kept a while, so going back is
//! instant), and every [`KEffect`]: API calls in tasks, log streams, kubectl
//! and plugins, and the ones that take the terminal (a shell, `$EDITOR`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hyper::Method;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::forward::Forwards;
use super::resources::Resource;
use super::state::{CONTAINERS, KEffect, KMsg, LogTarget, ObjRef, WatchKey};
use super::{auth, informer, ops, uiconfig, yaml};
use crate::ui::app::{App, Msg};
use crate::ui::client::{Endpoint, Lines, enc};

/// Informers kept after their view went away.
const KEEP_UNUSED: usize = 3;
/// How often pod and node metrics are fetched while shown.
const METRICS_EVERY: Duration = Duration::from_secs(15);

pub struct Rt {
    pub ep: Endpoint,
    informers: HashMap<WatchKey, (JoinHandle<()>, Instant)>,
    logs: HashMap<u64, JoinHandle<()>>,
    procs: HashMap<u64, JoinHandle<()>>,
    pub forwards: Forwards,
    tasks: Vec<JoinHandle<()>>,
    metrics: Option<(String, JoinHandle<()>)>,
    tx: UnboundedSender<Msg>,
}

impl Rt {
    pub fn new(ep: Endpoint, tx: UnboundedSender<Msg>) -> Rt {
        Rt { ep, informers: HashMap::new(), logs: HashMap::new(), procs: HashMap::new(), forwards: Forwards::default(), tasks: Vec::new(), metrics: None, tx }
    }

    /// The UI's settings and command history (once, at start).
    pub fn load_settings(&self) {
        let _ = self.tx.send(Msg::Kube(KMsg::Config(uiconfig::load())));
        if let Ok(text) = std::fs::read_to_string(uiconfig::history_path()) {
            let _ = self.tx.send(Msg::Kube(KMsg::History(text.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())));
        }
    }

    /// Another cluster: nothing of the old one keeps running.
    pub fn reset(&mut self, ep: Endpoint) {
        self.stop_all();
        self.ep = ep;
    }

    pub fn stop_all(&mut self) {
        for (_, (t, _)) in self.informers.drain() {
            t.abort();
        }
        for (_, t) in self.logs.drain().chain(self.procs.drain()) {
            t.abort();
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        if let Some((_, t)) = self.metrics.take() {
            t.abort();
        }
        self.forwards.stop_all();
    }

    fn spawn(&mut self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.retain(|t| !t.is_finished());
        self.tasks.push(tokio::spawn(f));
    }

    fn done(&self) -> impl Fn(Result<String>) + Send + Clone + 'static {
        let tx = self.tx.clone();
        move |r: Result<String>| {
            let _ = tx.send(Msg::Done(r.map_err(|e| format!("{e:#}"))));
        }
    }

    fn pager(&self, id: u64) -> impl Fn(Result<Vec<String>>) + Send + 'static {
        let tx = self.tx.clone();
        move |r: Result<Vec<String>>| {
            let msg = match r {
                Ok(lines) => KMsg::Pager { id, lines, append: false, done: true, error: None },
                Err(e) => KMsg::Pager { id, lines: Vec::new(), append: false, done: true, error: Some(format!("{e:#}")) },
            };
            let _ = tx.send(Msg::Kube(msg));
        }
    }

    fn watch(&mut self, wanted: Vec<(Resource, WatchKey)>, app: &App) {
        let now = Instant::now();
        for (res, key) in &wanted {
            match self.informers.get_mut(key) {
                Some((t, used)) if !t.is_finished() => *used = now,
                _ => {
                    let t = informer::spawn(self.ep.clone(), res.clone(), key.clone(), self.tx.clone());
                    self.informers.insert(key.clone(), (t, now));
                }
            }
        }
        // Unused ones: the most recent few stay.
        let mut unused: Vec<(WatchKey, Instant)> = self.informers.iter().filter(|(k, _)| !wanted.iter().any(|(_, w)| w == *k)).map(|(k, (_, u))| (k.clone(), *u)).collect();
        unused.sort_by_key(|(_, u)| std::cmp::Reverse(*u));
        for (k, _) in unused.into_iter().skip(KEEP_UNUSED) {
            if let Some((t, _)) = self.informers.remove(&k) {
                t.abort();
            }
        }
        self.metrics_for(&wanted, app);
    }

    /// Pod and node usage from metrics.k8s.io while such a table shows.
    fn metrics_for(&mut self, wanted: &[(Resource, WatchKey)], app: &App) {
        let has = app.kube.catalog.list.iter().any(|r| r.group == "metrics.k8s.io");
        let scope = wanted.iter().rev().find(|(r, _)| r.group.is_empty() && (r.plural == "pods" || r.plural == "nodes")).map(|(r, k)| (r.plural.clone(), k.ns.clone()));
        let id = scope.as_ref().map(|(p, ns)| format!("{p}/{}", ns.as_deref().unwrap_or("")));
        if !has || id.is_none() {
            if let Some((_, t)) = self.metrics.take() {
                t.abort();
            }
            return;
        }
        let id = id.unwrap();
        if self.metrics.as_ref().is_some_and(|(i, t)| *i == id && !t.is_finished()) {
            return;
        }
        if let Some((_, t)) = self.metrics.take() {
            t.abort();
        }
        let (plural, ns) = scope.unwrap();
        let (ep, tx) = (self.ep.clone(), self.tx.clone());
        let t = tokio::spawn(async move {
            let nodes = plural == "nodes";
            let kind = if nodes { "Node" } else { "Pod" };
            loop {
                if let Ok((cpu, mem)) = ops::metrics(&ep, nodes, ns.as_deref()).await {
                    let _ = tx.send(Msg::Kube(KMsg::Ext { kind: kind.into(), column: "CPU".into(), values: cpu.into_iter().collect() }));
                    if tx.send(Msg::Kube(KMsg::Ext { kind: kind.into(), column: "MEM".into(), values: mem.into_iter().collect() })).is_err() {
                        return;
                    }
                }
                tokio::time::sleep(METRICS_EVERY).await;
            }
        });
        self.metrics = Some((id, t));
    }
}

/// kubectl's arguments for this UI's cluster and namespace.
fn kubectl_base(app: &App, args: &[String]) -> Vec<String> {
    let ctx = &app.kube_context;
    let known = auth::load().ok().is_some_and(|c| c.named("contexts", ctx).is_some());
    let mut v = if known && !ctx.is_empty() { vec!["--context".to_string(), ctx.clone()] } else { vec!["--server".to_string(), app.kube_endpoint.clone()] };
    let has_ns = args.iter().any(|a| a == "-n" || a.starts_with("--namespace") || a == "-A" || a == "--all-namespaces" || (a.starts_with("-n") && a.len() > 2 && !a.starts_with("--")));
    if let (false, Some(ns)) = (has_ns, &app.kube.namespace) {
        v.push("-n".into());
        v.push(ns.clone());
    }
    v
}

/// Whether kubectl needs the terminal (`exec -it`, `edit`, `attach`).
pub fn interactive(args: &[String]) -> bool {
    let first = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("");
    let tty = args.iter().any(|a| matches!(a.as_str(), "-it" | "-ti" | "--tty" | "-t" | "--stdin" | "-i") || a == "--stdin=true" || a == "--tty=true");
    matches!(first, "edit" | "attach") || tty
}

/// Stream a command's output into pager `id`.
fn stream_command(mut cmd: tokio::process::Command, id: u64, tx: UnboundedSender<Msg>) -> JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Kube(KMsg::Pager { id, lines: Vec::new(), append: true, done: true, error: Some(format!("cannot run it: {e}")) }));
                return;
            }
        };
        async fn pump<R: tokio::io::AsyncRead + Unpin>(r: Option<R>, id: u64, tx: UnboundedSender<Msg>) {
            let Some(r) = r else { return };
            let mut lines = tokio::io::BufReader::new(r).lines();
            let mut batch = Vec::new();
            loop {
                let next = if batch.is_empty() { Ok(lines.next_line().await) } else { tokio::time::timeout(Duration::from_millis(50), lines.next_line()).await };
                match next {
                    Ok(Ok(Some(l))) => {
                        batch.push(crate::ui::docker::printable(l.as_bytes()));
                        if batch.len() < 2000 {
                            continue;
                        }
                    }
                    Ok(_) => break,
                    Err(_) => {}
                }
                let _ = tx.send(Msg::Kube(KMsg::Pager { id, lines: std::mem::take(&mut batch), append: true, done: false, error: None }));
            }
            if !batch.is_empty() {
                let _ = tx.send(Msg::Kube(KMsg::Pager { id, lines: batch, append: true, done: false, error: None }));
            }
        }
        let (a, b) = (tokio::spawn(pump(child.stdout.take(), id, tx.clone())), tokio::spawn(pump(child.stderr.take(), id, tx.clone())));
        let status = child.wait().await;
        let _ = tokio::join!(a, b);
        let error = match status {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("exited with {s}")),
            Err(e) => Some(e.to_string()),
        };
        let _ = tx.send(Msg::Kube(KMsg::Pager { id, lines: Vec::new(), append: true, done: true, error }));
    })
}

/// Copy to the clipboard: OSC 52 (works over SSH, in most terminals and
/// tmux with set-clipboard), and pbcopy on a Mac's own terminal.
pub fn copy(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = if std::env::var_os("TMUX").is_some() { format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\") } else { format!("\x1b]52;c;{b64}\x07") };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
    if cfg!(target_os = "macos") && std::env::var_os("SSH_CONNECTION").is_none() {
        if let Ok(mut c) = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            let _ = c.wait();
        }
    }
}

/// Where saved logs and pagers go.
pub fn save(name: &str, lines: &[String]) -> Result<String> {
    let dir = std::env::temp_dir().join("litek8s");
    std::fs::create_dir_all(&dir)?;
    let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    let clean: String = clean.trim_matches('_').chars().take(80).collect();
    let path = dir.join(format!("{clean}-{}.txt", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    std::fs::write(&path, lines.join("\n") + "\n").with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path.display().to_string())
}

/// Replace `$NAME`-style variables (longest names first).
pub fn expand(s: &str, vars: &[(String, String)]) -> String {
    let mut v: Vec<&(String, String)> = vars.iter().collect();
    v.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    let mut out = s.to_string();
    for (k, val) in v {
        out = out.replace(&format!("${k}"), val).replace(&format!("${{{k}}}"), val);
    }
    out
}

/// Follow logs: one stream per pod and container, lines tagged with their
/// source, all in one task (aborting it stops them).
async fn logs(ep: Endpoint, id: u64, target: LogTarget, previous: bool, tail: u32, max: usize, tx: UnboundedSender<Msg>) {
    let pods_res = Resource { plural: "pods".into(), version: "v1".into(), kind: "Pod".into(), namespaced: true, ..Default::default() };
    let sources: Result<Vec<(String, String, String)>> = async {
        let pods: Vec<Value> = match &target {
            LogTarget::Pod { ns, name, .. } => vec![ops::get(&ep, &ObjRef { res: pods_res.clone(), ns: ns.clone(), name: name.clone() }).await?],
            LogTarget::Owner(obj) => ops::pods_of(&ep, obj).await?,
        };
        let mut out = Vec::new();
        for p in &pods {
            let ns = p.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = p.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut cs: Vec<String> = p.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
            if let LogTarget::Pod { container: Some(c), .. } = &target {
                cs = vec![c.clone()];
            }
            for c in cs {
                out.push((ns.clone(), name.clone(), c));
            }
        }
        Ok(out)
    }
    .await;
    let sources = match sources {
        Ok(s) if s.is_empty() => {
            let _ = tx.send(Msg::Kube(KMsg::LogSources { id, sources: vec!["-".into()] }));
            let _ = tx.send(Msg::Kube(KMsg::LogEnd { id, src: 0, error: Some("no pods".into()) }));
            return;
        }
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Msg::Kube(KMsg::LogSources { id, sources: vec!["-".into()] }));
            let _ = tx.send(Msg::Kube(KMsg::LogEnd { id, src: 0, error: Some(format!("{e:#}")) }));
            return;
        }
    };
    let total = sources.len();
    let sources: Vec<(String, String, String)> = sources.into_iter().take(max).collect();
    let pods: std::collections::BTreeSet<&str> = sources.iter().map(|(_, p, _)| p.as_str()).collect();
    let one_pod = pods.len() == 1;
    let labels: Vec<String> = sources.iter().map(|(_, p, c)| if one_pod { c.clone() } else { format!("{p}/{c}") }).collect();
    let _ = tx.send(Msg::Kube(KMsg::LogSources { id, sources: labels }));
    if total > max {
        let _ = tx.send(Msg::Done(Err(format!("showing {max} of {total} streams (logs.maxStreams in ui.yaml)"))));
    }
    let mut set = tokio::task::JoinSet::new();
    for (src, (ns, pod, c)) in sources.into_iter().enumerate() {
        let (ep, tx) = (ep.clone(), tx.clone());
        set.spawn(async move {
            let mut path = format!("/api/v1/namespaces/{ns}/pods/{pod}/log?timestamps=true&tailLines={tail}&container={}", enc(&c));
            path.push_str(if previous { "&previous=true" } else { "&follow=true" });
            let r: Result<()> = async {
                let resp = ep.send(Method::GET, &path, None, Vec::new()).await?;
                let status = resp.status();
                if !status.is_success() {
                    let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
                    crate::ui::client::check(status, &body)?;
                    anyhow::bail!("{status}");
                }
                let mut lines = Lines::new(resp.into_body());
                let mut batch = Vec::new();
                loop {
                    let next = if batch.is_empty() { Ok(lines.next().await) } else { tokio::time::timeout(Duration::from_millis(50), lines.next()).await };
                    match next {
                        Ok(Some(l)) => {
                            let mut s = crate::ui::docker::printable(&l);
                            if s.len() > 16_384 {
                                let mut cut = 16_384;
                                while !s.is_char_boundary(cut) {
                                    cut -= 1;
                                }
                                s.truncate(cut);
                                s.push('…');
                            }
                            batch.push(s);
                            if batch.len() < 1000 {
                                continue;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {}
                    }
                    if tx.send(Msg::Kube(KMsg::LogLines { id, src, lines: std::mem::take(&mut batch) })).is_err() {
                        return Ok(());
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(Msg::Kube(KMsg::LogLines { id, src, lines: batch }));
                }
                Ok(())
            }
            .await;
            let _ = tx.send(Msg::Kube(KMsg::LogEnd { id, src, error: r.err().map(|e| format!("{e:#}")) }));
        });
    }
    while set.join_next().await.is_some() {}
}

/// Carry out one effect of the Kubernetes view.
pub async fn effect(run: &mut crate::ui::Runtime, e: KEffect, terminal: &mut crate::ui::Term, app: &mut App) -> Result<()> {
    let rt = &mut run.kube_rt;
    let ep = rt.ep.clone();
    let tx = rt.tx.clone();
    let done = rt.done();
    match e {
        KEffect::Watch(wanted) => rt.watch(wanted, app),
        KEffect::Relist(key) => {
            if let Some((t, _)) = rt.informers.remove(&key) {
                t.abort();
            }
            let wanted = app.kube.wanted();
            rt.watch(wanted, app);
        }
        KEffect::Discover => {
            rt.spawn(async move {
                let r = ops::discover(&ep).await.map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Kube(KMsg::Discovery(r)));
            });
        }
        KEffect::LoadContexts => {
            let (list, current) = match auth::load() {
                Ok(c) => (c.contexts(), c.current()),
                Err(e) => {
                    app.flash(format!("{e:#}"), true);
                    (Vec::new(), None)
                }
            };
            app.kube.apply(KMsg::Contexts { list, current });
        }
        KEffect::Describe { id, obj } => {
            let out = rt.pager(id);
            rt.spawn(async move { out(ops::describe(&ep, &obj).await) });
        }
        KEffect::Yaml { id, obj, managed } => {
            let out = rt.pager(id);
            rt.spawn(async move { out(ops::yaml(&ep, &obj, managed).await) });
        }
        KEffect::Decode { id, obj } => {
            let out = rt.pager(id);
            rt.spawn(async move { out(ops::decode(&ep, &obj).await) });
        }
        KEffect::History { id, obj } => {
            let out = rt.pager(id);
            rt.spawn(async move { out(ops::history(&ep, &obj).await) });
        }
        KEffect::Edit { obj, text } => edit(run, terminal, app, obj, text).await?,
        KEffect::Apply { obj, body } => {
            rt.spawn(async move {
                let r: Result<String> = async {
                    let v: Value = serde_yaml::from_str(&body).context("not valid YAML")?;
                    ep.json(Method::PUT, &obj.res.object_path(&obj.ns, &obj.name), Some(&v)).await?;
                    Ok(format!("{} edited", obj.label()))
                }
                .await;
                done(r);
            });
        }
        KEffect::Delete { objs, propagation, force } => {
            app.flash(format!("deleting {}…", if objs.len() == 1 { objs[0].label() } else { format!("{} objects", objs.len()) }), false);
            rt.spawn(async move { done(ops::delete(&ep, &objs, propagation, force).await) });
        }
        KEffect::Scale { obj, replicas } => rt.spawn(async move { done(ops::scale(&ep, &obj, replicas).await) }),
        KEffect::Restart(objs) => rt.spawn(async move { done(ops::restart(&ep, &objs).await) }),
        KEffect::Undo(obj) => rt.spawn(async move { done(ops::undo(&ep, &obj).await) }),
        KEffect::Cordon { nodes, on } => rt.spawn(async move { done(ops::cordon(&ep, &nodes, on).await) }),
        KEffect::Drain { nodes } => {
            app.flash(format!("draining {}…", nodes.join(", ")), false);
            let progress_tx = tx.clone();
            rt.spawn(async move {
                let progress = move |m: String| {
                    let _ = progress_tx.send(Msg::Done(Ok(m)));
                };
                done(ops::drain(&ep, &nodes, progress).await)
            });
        }
        KEffect::Trigger(obj) => rt.spawn(async move { done(ops::trigger(&ep, &obj).await) }),
        KEffect::Containers { obj, purpose } => {
            rt.spawn(async move {
                match ops::get(&ep, &obj).await {
                    Ok(p) => {
                        let list = ops::containers(&p);
                        let _ = tx.send(Msg::Kube(KMsg::Containers { obj, list, purpose }));
                    }
                    Err(e) => done(Err(e)),
                }
            });
        }
        KEffect::Logs { id, target, previous, tail, max_streams } => {
            if let Some(t) = rt.logs.remove(&id) {
                t.abort();
            }
            rt.logs.insert(id, tokio::spawn(logs(ep, id, target, previous, tail, max_streams, tx)));
        }
        KEffect::StopLogs(id) => {
            if let Some(t) = rt.logs.remove(&id) {
                t.abort();
            }
        }
        KEffect::Shell { ns, pod, container } => {
            let what = match &container {
                Some(c) => format!("{ns}/{pod} [{c}]"),
                None => format!("{ns}/{pod}"),
            };
            run.suspend(terminal, app)?;
            print!("\r\n\x1b[1mlitek8s ui\x1b[0m: shell in {what}; exit the shell to come back.\r\n\r\n");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let r = super::exec::shell(&ep, &ns, &pod, container.as_deref()).await;
            run.resume(terminal, app)?;
            match r {
                Ok(code) => app.flash(format!("shell in {what} exited ({code})"), code != 0 && code != 130),
                Err(e) => app.flash(format!("shell in {what}: {e:#}"), true),
            }
        }
        KEffect::PortForwardPrepare(obj) => {
            rt.spawn(async move {
                match ops::ports(&ep, &obj).await {
                    Ok(ports) => {
                        let _ = tx.send(Msg::Kube(KMsg::Ports { obj, ports }));
                    }
                    Err(e) => done(Err(e)),
                }
            });
        }
        KEffect::PortForward { obj, local, remote } => {
            let forwards = rt.forwards.clone();
            rt.spawn(async move {
                let r = async {
                    let (pod, port) = ops::forward_target(&ep, &obj, &remote).await?;
                    let target = format!("{}/{}", obj.res.short.first().cloned().unwrap_or_else(|| obj.res.singular.clone()), obj.name);
                    forwards.start(ep, obj.ns.clone(), pod, target, local, port, tx.clone()).await
                }
                .await;
                let _ = tx.send(Msg::Kube(KMsg::Forwards(forwards.list())));
                done(r);
            });
        }
        KEffect::StopForward(local) => {
            if rt.forwards.stop(local) {
                app.flash(format!("port-forward 127.0.0.1:{local} stopped"), false);
            }
            let list = rt.forwards.list();
            app.kube.apply(KMsg::Forwards(list));
        }
        KEffect::Kubectl { id, args } => {
            let words = super::filter::words(&args);
            let bin = app.kube.config.kubectl.clone().unwrap_or_else(|| "kubectl".into());
            let mut full = kubectl_base(app, &words);
            full.extend(words.iter().cloned());
            if interactive(&words) {
                run.suspend(terminal, app)?;
                let _ = ratatui::crossterm::terminal::disable_raw_mode();
                print!("\x1b[1mlitek8s ui\x1b[0m: {bin} {}\n\n", full.join(" "));
                let status = std::process::Command::new(&bin).args(&full).status();
                let _ = ratatui::crossterm::terminal::enable_raw_mode();
                run.resume(terminal, app)?;
                let text = match status {
                    Ok(s) => format!("{bin} {args}: {s}"),
                    Err(e) => format!("cannot run {bin}: {e}"),
                };
                app.kube.apply(KMsg::Pager { id, lines: vec![text], append: false, done: true, error: None });
            } else {
                let mut cmd = tokio::process::Command::new(&bin);
                cmd.args(&full);
                let t = stream_command(cmd, id, run.kube_rt.tx.clone());
                run.kube_rt.procs.insert(id, t);
            }
        }
        KEffect::StopKubectl(id) => {
            if let Some(t) = rt.procs.remove(&id) {
                t.abort();
            }
        }
        KEffect::DrillDown(obj) => {
            rt.spawn(async move {
                match ops::drill(&ep, &obj).await {
                    Ok((ns, labels, fields, title)) => {
                        let _ = tx.send(Msg::Kube(KMsg::Open { res: "v1/pods".into(), ns, labels, fields, title }));
                    }
                    Err(e) => done(Err(e)),
                }
            });
        }
        KEffect::Xray { id, res, ns } => {
            let pods = app.kube.catalog.find("pods").cloned().unwrap_or_default();
            let kinds: HashMap<String, Resource> = ["ReplicaSet", "Job"].iter().filter_map(|k| app.kube.catalog.list.iter().find(|r| r.kind == *k && (r.group == "apps" || r.group == "batch")).map(|r| (k.to_string(), r.clone()))).collect();
            rt.spawn(async move {
                let r = ops::xray(&ep, &pods, &kinds, &res, ns.as_deref()).await;
                let msg = match r {
                    Ok(nodes) => KMsg::Xray { id, nodes, error: None },
                    Err(e) => KMsg::Xray { id, nodes: Vec::new(), error: Some(format!("{e:#}")) },
                };
                let _ = tx.send(Msg::Kube(msg));
            });
        }
        KEffect::Copy(text) => copy(&text),
        KEffect::CopyYaml(obj) => {
            let obj = if obj.res.plural == CONTAINERS { return Ok(()) } else { obj };
            match ops::yaml(&ep, &obj, false).await {
                Ok(lines) => {
                    copy(&(lines.join("\n") + "\n"));
                    app.flash(format!("copied the YAML of {}", obj.label()), false);
                }
                Err(e) => app.flash(format!("{e:#}"), true),
            }
        }
        KEffect::Save { name, lines } => match save(&name, &lines) {
            Ok(p) => app.flash(format!("saved {} lines to {p}", lines.len()), false),
            Err(e) => app.flash(format!("{e:#}"), true),
        },
        KEffect::Plugin { plugin, vars } => {
            let mut vars = vars;
            vars.push(("CONTEXT".into(), app.kube_context.clone()));
            vars.push(("KUBECONFIG".into(), std::env::var("KUBECONFIG").unwrap_or_default()));
            let args: Vec<String> = plugin.args.iter().map(|a| expand(a, &vars)).collect();
            let command = expand(&plugin.command, &vars);
            if plugin.background {
                let id = app.kube.open_output(format!("{} {}", plugin.name, args.join(" ")));
                let mut cmd = tokio::process::Command::new(&command);
                cmd.args(&args).envs(vars.iter().filter(|(k, _)| !k.starts_with("COL-")).cloned());
                let t = stream_command(cmd, id, tx);
                rt.procs.insert(id, t);
            } else {
                run.suspend(terminal, app)?;
                let _ = ratatui::crossterm::terminal::disable_raw_mode();
                let status = std::process::Command::new(&command).args(&args).envs(vars.iter().filter(|(k, _)| !k.starts_with("COL-")).cloned()).status();
                let _ = ratatui::crossterm::terminal::enable_raw_mode();
                run.resume(terminal, app)?;
                match status {
                    Ok(s) if s.success() => app.flash(format!("{} done", plugin.name), false),
                    Ok(s) => app.flash(format!("{}: {s}", plugin.name), true),
                    Err(e) => app.flash(format!("cannot run {command}: {e}"), true),
                }
            }
        }
        KEffect::SaveHistory(h) => {
            let p = uiconfig::history_path();
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(&p, h.join("\n") + "\n");
        }
    }
    Ok(())
}

/// `kubectl edit`, checked by the server first: the object as YAML in
/// `$EDITOR`; what comes back is sent as a dry run, and errors come back
/// into the file as comments; a valid change shows as a diff to apply.
async fn edit(run: &mut crate::ui::Runtime, terminal: &mut crate::ui::Term, app: &mut App, obj: ObjRef, text: Option<String>) -> Result<()> {
    let ep = run.kube_rt.ep.clone();
    let current = match ops::get(&ep, &obj).await {
        Ok(o) => o,
        Err(e) => {
            app.flash(format!("{e:#}"), true);
            return Ok(());
        }
    };
    let original = yaml::to_yaml(&current, false);
    let mut content = text.unwrap_or_else(|| original.clone());
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let dir = std::env::temp_dir().join("litek8s");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("edit-{}-{}.yaml", obj.name.replace('/', "_"), std::process::id()));
    let header = format!(
        "# Editing {} in context {}. Lines starting with '#' are ignored;\n# an empty file cancels. The server checks it (dry run) before anything changes.\n",
        obj.label(),
        app.kube_context
    );
    let mut error: Option<String> = None;
    run.suspend(terminal, app)?;
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    let outcome: Result<Option<(String, Vec<String>)>> = async {
        loop {
            let mut file = header.clone();
            if let Some(e) = &error {
                file.push_str("#\n");
                for l in e.lines() {
                    file.push_str(&format!("# Error: {l}\n"));
                }
                file.push_str("#\n");
            }
            file.push_str(&content);
            std::fs::write(&path, &file)?;
            let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status().with_context(|| format!("cannot run {editor}"))?;
            if !status.success() {
                anyhow::bail!("{editor} exited with {status}; nothing changed");
            }
            let back = std::fs::read_to_string(&path)?;
            let body: String = back.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
            if body.trim().is_empty() {
                return Ok(None);
            }
            if body.trim() == original.trim() {
                return Ok(None);
            }
            content = body.clone() + "\n";
            let v: Value = match serde_yaml::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    error = Some(format!("not valid YAML: {e}"));
                    continue;
                }
            };
            let path_dry = format!("{}?dryRun=All", obj.res.object_path(&obj.ns, &obj.name));
            match ep.json(Method::PUT, &path_dry, Some(&v)).await {
                Ok(_) => {
                    let diff = yaml::unified(&original, &yaml::to_yaml(&v, false), 3);
                    return Ok(Some((content.clone(), diff)));
                }
                Err(e) => error = Some(format!("{e:#}")),
            }
        }
    }
    .await;
    let _ = std::fs::remove_file(&path);
    let _ = ratatui::crossterm::terminal::enable_raw_mode();
    run.resume(terminal, app)?;
    match outcome {
        Ok(None) => app.flash("edit cancelled: no changes", false),
        Ok(Some((body, diff))) => app.kube.apply(KMsg::Diff { obj, lines: diff, body }),
        Err(e) => app.flash(format!("{e:#}"), true),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubectl_needs_the_terminal_for_interactive_commands() {
        let w = |s: &str| super::super::filter::words(s);
        assert!(interactive(&w("exec -it web -- sh")));
        assert!(interactive(&w("edit deploy web")));
        assert!(!interactive(&w("get pods -o wide")));
        assert!(!interactive(&w("logs -f web")));
    }

    #[test]
    fn plugin_variables() {
        let vars = vec![("NAME".to_string(), "web".to_string()), ("NAMESPACE".to_string(), "prod".to_string()), ("COL-IMAGE".to_string(), "nginx".to_string())];
        assert_eq!(expand("$NAMESPACE/$NAME ${NAME} $COL-IMAGE", &vars), "prod/web web nginx");
    }
}
