//! `litek8s ui`: a terminal UI for the local dev environment, what
//! OrbStack's and Docker Desktop's windows give their users: the dev
//! cluster's state and resources, Docker containers (grouped by compose
//! project) with logs, shells and actions, images and volumes, a free-space
//! view, settings, and a light Kubernetes view. It works over SSH and talks
//! only to litek8s's public APIs (the Docker socket, the Kubernetes API,
//! the dev supervisor's state and CLI), so it shows a remote litek8s the
//! same way. See docs/ui.md.

pub mod app;
mod client;
mod clusters;
mod ctx;
mod docker;
mod exec;
mod home;
mod kube;
pub mod model;
pub mod view;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use app::{App, DevOp, Effect, LogSource, Msg, Tab};
use client::Endpoint;

#[derive(Debug, Args)]
pub struct UiArgs {
    /// Docker API to show (default: the local cluster's socket, ~/.litek8s/docker.sock for `dev`)
    #[arg(long, value_name = "URL")]
    pub docker_host: Option<String>,
    /// kubeconfig context to show (default: the local dev cluster)
    #[arg(long)]
    pub context: Option<String>,
    /// The local cluster to show (default: the one shown last, else `dev`)
    #[arg(long)]
    pub cluster: Option<String>,
    /// Start in this namespace of the Kubernetes view
    #[arg(long, short)]
    pub namespace: Option<String>,
    /// Use the mouse (clicks and wheel; takes over terminal text selection)
    #[arg(long)]
    pub mouse: bool,
}

type Term = ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

pub async fn run(args: UiArgs) -> Result<()> {
    use std::io::IsTerminal;
    let started = Instant::now();
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("litek8s ui needs a terminal");
    }
    let cluster = match &args.cluster {
        Some(c) => crate::dev::cluster::Cluster::new(c)?,
        None => {
            let home = crate::dev::dev_home().unwrap_or_default();
            ctx::active_cluster()
                .and_then(|c| crate::dev::cluster::Cluster::new(&c).ok())
                .filter(|c| c.exists(&home))
                .unwrap_or_else(crate::dev::cluster::Cluster::default_cluster)
        }
    };
    let socket = cluster.docker_socket();
    let docker_ep = match &args.docker_host {
        Some(h) => Endpoint::parse_docker_host(h)?,
        None => Endpoint::unix(socket.clone()),
    };
    let local = matches!(&docker_ep.transport, client::Transport::Unix(p) if *p == socket) && args.context.is_none();
    // The local dev cluster's API follows its state (it moves when the port
    // changes, and is not there while it is down); a context is fixed.
    let fixed = if local {
        None
    } else {
        match kube::resolve(args.context.as_deref()) {
            Ok(r) => Some(r),
            Err(e) if args.context.is_some() => return Err(e),
            Err(_) => None,
        }
    };
    let (kube_ep, kube_context) = match &fixed {
        Some((ep, name)) => (ep.clone(), name.clone()),
        None => (dev_endpoint(crate::dev::DEFAULT_PORT), cluster.context()),
    };

    let (tx, rx) = unbounded_channel();
    let mut app = App::new(docker_ep.display.clone(), kube_ep.display.clone(), kube_context);
    app.mouse = args.mouse;
    app.namespace = args.namespace.clone();
    app.cluster = cluster.name.clone();
    app.apply(Msg::Config(crate::dev::config::DevConfig::load().map_err(|e| format!("{e:#}"))));

    let refresh = Arc::new(Notify::new());
    let pace = Arc::new(AtomicU64::new(home::VISIBLE));
    let wake_home = Arc::new(Notify::new());
    let active = Arc::new(Mutex::new(cluster.name.clone()));
    let clusters_visible = Arc::new(AtomicBool::new(false));
    let wake_clusters = Arc::new(Notify::new());
    let docker_task = docker::spawn(docker_ep.clone(), tx.clone(), refresh.clone());
    let tasks = vec![
        home::spawn(tx.clone(), local, pace.clone(), wake_home.clone(), active.clone(), fixed.as_ref().filter(|_| !local).map(|(ep, _)| ep.clone())),
        clusters::spawn(tx.clone(), clusters_visible.clone(), wake_clusters.clone()),
    ];
    let kube_watches = if fixed.is_some() { kube::spawn_watches(kube_ep.clone(), tx.clone()) } else { Vec::new() };
    if fixed.is_none() {
        let _ = tx.send(Msg::KubeDown(DEV_DOWN.into()));
    }

    let mut terminal = setup(args.mouse)?;
    let input = Input::new();
    let mut rt = Runtime {
        docker: docker_ep,
        docker_task,
        local,
        active,
        clusters_visible,
        wake_clusters,
        kube: kube_ep,
        follow_dev: fixed.is_none(),
        kube_port: None,
        kube_watches,
        tx,
        refresh,
        wake_home,
        forwards: kube::Forwards::default(),
        log_task: None,
        input,
        stats: Stats { started, first_frame: None, data_frame: None, containers_seen: false, frames: 0 },
    };
    let result = rt.event_loop(&mut terminal, &mut app, rx, &pace).await;
    rt.forwards.stop_all();
    if let Some(t) = rt.log_task.take() {
        t.abort();
    }
    rt.docker_task.abort();
    for t in tasks.into_iter().chain(rt.kube_watches.drain(..)) {
        t.abort();
    }
    restore(args.mouse);
    if std::env::var_os("LITEK8S_UI_STATS").is_some() {
        let s = &rt.stats;
        let ms = |d: Option<Duration>| d.map(|d| format!("{:.1} ms", d.as_secs_f64() * 1000.0)).unwrap_or_else(|| "-".into());
        eprintln!(
            "litek8s ui: first frame {}, first frame with containers {}, {} frames in {:.1} s",
            ms(s.first_frame),
            ms(s.data_frame),
            s.frames,
            s.started.elapsed().as_secs_f64()
        );
    }
    result
}

/// How often ages ("5m") are redrawn when nothing else changes.
const AGES_EVERY: Duration = Duration::from_secs(30);

/// `LITEK8S_UI_STATS=1`: what the UI did, printed when it exits.
struct Stats {
    started: Instant,
    first_frame: Option<Duration>,
    /// The first frame showing the engine's containers.
    data_frame: Option<Duration>,
    containers_seen: bool,
    frames: u64,
}

const DEV_DOWN: &str = "the cluster is not running (Home: u starts it)";

fn dev_endpoint(port: u16) -> Endpoint {
    Endpoint::parse_http(&format!("http://127.0.0.1:{port}"), None).expect("static URL")
}

fn setup(mouse: bool) -> Result<Term> {
    terminal::enable_raw_mode().context("cannot put the terminal in raw mode")?;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
    if mouse {
        execute!(std::io::stdout(), event::EnableMouseCapture)?;
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(mouse);
        hook(info);
    }));
    // Clear with a plain escape sequence: ratatui's `Terminal::clear` first asks
    // the terminal for the cursor position, and terminals that never answer
    // that query (some embedded or non-interactive ptys) fail the start with
    // "the cursor position could not be read". A new Terminal draws its first
    // frame in full anyway.
    execute!(std::io::stdout(), terminal::Clear(terminal::ClearType::All))?;
    let mut t = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    t.hide_cursor()?;
    Ok(t)
}

fn restore(mouse: bool) {
    if mouse {
        let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    }
    let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// Terminal events, from crossterm's event stream: it waits in the
/// terminal reader (no timer), and dropping it stops reading, so a shell or
/// an editor gets every keystroke while the UI is suspended.
struct Input {
    stream: Option<event::EventStream>,
}

impl Input {
    fn new() -> Input {
        Input { stream: Some(event::EventStream::new()) }
    }

    /// The next event; never ready while paused; `None` when the terminal
    /// is gone.
    async fn next(&mut self) -> Option<Event> {
        use futures_util::StreamExt;
        match &mut self.stream {
            Some(s) => s.next().await.and_then(Result::ok),
            None => std::future::pending().await,
        }
    }

    fn pause(&mut self) {
        self.stream = None;
    }

    fn resume(&mut self) {
        // Whatever was typed for the shell is not for the UI.
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let _ = event::read();
        }
        self.stream = Some(event::EventStream::new());
    }
}

struct Runtime {
    docker: Endpoint,
    docker_task: tokio::task::JoinHandle<()>,
    /// The views show a local cluster (not a --docker-host / --context).
    local: bool,
    /// The local cluster shown (Home samples it).
    active: Arc<Mutex<String>>,
    clusters_visible: Arc<AtomicBool>,
    wake_clusters: Arc<Notify>,
    kube: Endpoint,
    /// The Kubernetes API is the local dev cluster's, wherever it runs.
    follow_dev: bool,
    kube_port: Option<u16>,
    kube_watches: Vec<tokio::task::JoinHandle<()>>,
    tx: UnboundedSender<Msg>,
    refresh: Arc<Notify>,
    wake_home: Arc<Notify>,
    forwards: kube::Forwards,
    log_task: Option<tokio::task::JoinHandle<()>>,
    input: Input,
    stats: Stats,
}

impl Runtime {
    async fn event_loop(&mut self, terminal: &mut Term, app: &mut App, mut rx: UnboundedReceiver<Msg>, pace: &AtomicU64) -> Result<()> {
        // Drawn only when a key, a resize or new data changes something;
        // timers only for a status line to fade and for ages to move on.
        let mut dirty = true;
        let mut status_until: Option<tokio::time::Instant> = None;
        let mut last_status: Option<(String, bool)> = None;
        let mut ages_at = tokio::time::Instant::now() + AGES_EVERY;
        loop {
            if dirty {
                app.now = chrono::Utc::now().timestamp();
                terminal.draw(|f| view::draw(f, app))?;
                self.stats.frames += 1;
                if self.stats.first_frame.is_none() {
                    self.stats.first_frame = Some(self.stats.started.elapsed());
                }
                if self.stats.containers_seen && self.stats.data_frame.is_none() {
                    self.stats.data_frame = Some(self.stats.started.elapsed());
                }
                dirty = false;
                ages_at = tokio::time::Instant::now() + AGES_EVERY;
            }
            let shows_ages = matches!(app.tab, Tab::Images | Tab::Kube) && app.logs.is_none();
            let effects = tokio::select! {
                ev = self.input.next() => match ev {
                    Some(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if app.status.is_some() && app.overlay == app::Overlay::None {
                            app.status = None;
                        }
                        dirty = true;
                        app.key(k)
                    }
                    Some(Event::Mouse(m)) => {
                        dirty = true;
                        app.mouse(m)
                    }
                    // draw() picks up the new size.
                    Some(Event::Resize(..)) => {
                        dirty = true;
                        Vec::new()
                    }
                    Some(_) => continue,
                    None => return Ok(()),
                },
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    dirty |= self.visible(app, &msg);
                    app.apply(msg);
                    // Take what else is queued before drawing (log bursts).
                    while let Ok(m) = rx.try_recv() {
                        dirty |= self.visible(app, &m);
                        app.apply(m);
                    }
                    self.follow_dev_cluster(app);
                    // The shown cluster was deleted: the UI went back to `dev`.
                    if self.local && *self.active.lock().unwrap() != app.cluster {
                        let name = app.cluster.clone();
                        self.use_cluster(app, &name);
                    }
                    Vec::new()
                }
                _ = tokio::time::sleep_until(status_until.unwrap_or_else(tokio::time::Instant::now)), if status_until.is_some() => {
                    app.status = None;
                    last_status = None;
                    status_until = None;
                    dirty = true;
                    Vec::new()
                }
                _ = tokio::time::sleep_until(ages_at), if shows_ages => {
                    dirty = true;
                    Vec::new()
                }
            };
            let every = match app.tab {
                _ if app.logs.is_some() => home::HIDDEN,
                Tab::Home | Tab::Space => home::VISIBLE,
                Tab::Settings => home::SETTINGS,
                _ => home::HIDDEN,
            };
            if pace.swap(every, Ordering::Relaxed) > every {
                // Back on Home: a fresh sample now, not at the slow pace's end.
                self.wake_home.notify_one();
            }
            let on_clusters = app.tab == Tab::Clusters && app.logs.is_none();
            if self.clusters_visible.swap(on_clusters, Ordering::Relaxed) != on_clusters && on_clusters {
                self.wake_clusters.notify_one();
            }
            for e in effects {
                if self.effect(e, terminal, app).await? {
                    return Ok(());
                }
            }
            // A new status line shows for 8 s.
            if app.status != last_status {
                last_status = app.status.clone();
                status_until = last_status.as_ref().map(|_| tokio::time::Instant::now() + Duration::from_secs(8));
            }
        }
    }

    /// Whether a message can change what is on screen (Home samples arrive
    /// in the background for other views too).
    fn visible(&mut self, app: &App, msg: &Msg) -> bool {
        match msg {
            Msg::Containers(_) => {
                self.stats.containers_seen = true;
                true
            }
            Msg::Home(_) => matches!(app.tab, Tab::Home | Tab::Space | Tab::Settings) && app.logs.is_none(),
            Msg::Clusters(_) => app.tab == Tab::Clusters && app.logs.is_none(),
            Msg::LogLines(..) | Msg::LogEnd(..) => app.logs.is_some(),
            _ => true,
        }
    }

    /// Carry out one effect; `true` to quit.
    async fn effect(&mut self, e: Effect, terminal: &mut Term, app: &mut App) -> Result<bool> {
        let docker = self.docker.clone();
        let kube = self.kube.clone();
        let tx = self.tx.clone();
        let refresh = self.refresh.clone();
        let done = move |r: Result<String>| {
            let _ = tx.send(Msg::Done(r.map_err(|e| format!("{e:#}"))));
        };
        match e {
            Effect::Quit => return Ok(true),
            Effect::Refresh => {
                self.refresh.notify_one();
                self.wake_home.notify_one();
                self.wake_clusters.notify_one();
            }
            Effect::UseCluster(name) => self.use_cluster(app, &name),
            Effect::UseContext(name) => match kube::resolve(Some(&name)) {
                Ok((ep, ctx)) => {
                    self.follow_dev = false;
                    self.kube_port = None;
                    for t in self.kube_watches.drain(..) {
                        t.abort();
                    }
                    app.apply(Msg::KubeDown(format!("connecting to {ctx}…")));
                    self.kube = ep;
                    app.kube_endpoint = self.kube.display.clone();
                    app.kube_context = ctx;
                    self.kube_watches = kube::spawn_watches(self.kube.clone(), self.tx.clone());
                }
                Err(e) => app.flash(format!("{e:#}"), true),
            },
            Effect::SwitchTools(name) => {
                let cluster = crate::dev::cluster::Cluster { name };
                let port = crate::dev::cluster_state(&cluster.name).map(|s| s.port);
                let r = ctx::switch_kube(true, &cluster, port).and_then(|k| ctx::switch_docker(true, &cluster).map(|d| format!("{k}; {d}")));
                self.wake_home.notify_one();
                done(r);
            }
            Effect::Batch(list) => {
                // One after the other, in a task: a prune can take a while.
                app.flash("freeing space…", false);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let mut msgs = Vec::new();
                    let mut failed = false;
                    for e in list {
                        let r = match e {
                            Effect::PruneContainers => docker::prune_containers(&docker).await,
                            Effect::PruneBuildCache => docker::prune_build_cache(&docker).await,
                            Effect::RemoveImages(ids) => docker::remove_images(&docker, &ids).await,
                            Effect::RemoveVolumes(names) => docker::remove_volumes(&docker, &names).await,
                            _ => continue,
                        };
                        match r {
                            Ok(m) => msgs.push(m),
                            Err(e) => {
                                failed = true;
                                msgs.push(format!("{e:#}"));
                            }
                        }
                    }
                    let text = msgs.join("; ");
                    let _ = tx.send(Msg::Done(if failed { Err(text) } else { Ok(text) }));
                    refresh.notify_one();
                });
            }
            Effect::Containers(op, ids) => {
                app.flash(format!("{} …", op.verb()), false);
                tokio::spawn(async move { done(docker::containers(&docker, &op, &ids).await) });
            }
            Effect::RemoveImages(ids) => {
                tokio::spawn(async move { done(docker::remove_images(&docker, &ids).await) });
            }
            Effect::RemoveVolumes(names) => {
                tokio::spawn(async move { done(docker::remove_volumes(&docker, &names).await) });
            }
            Effect::PruneContainers => {
                tokio::spawn(async move { done(docker::prune_containers(&docker).await) });
            }
            Effect::PruneBuildCache => {
                tokio::spawn(async move { done(docker::prune_build_cache(&docker).await) });
            }
            Effect::Logs(source) => {
                if let Some(t) = self.log_task.take() {
                    t.abort();
                }
                let id = app.log_id().unwrap_or(0);
                let tx = self.tx.clone();
                self.log_task = Some(match source {
                    LogSource::Container { id: cid, .. } => tokio::spawn(docker::logs(docker, cid, id, tx)),
                    LogSource::Pod { namespace, name, container } => tokio::spawn(kube::logs(kube, namespace, name, container, id, tx)),
                });
            }
            Effect::StopLogs => {
                if let Some(t) = self.log_task.take() {
                    t.abort();
                }
            }
            Effect::Exec(id) => {
                let name = app.containers.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_else(|| id[..12.min(id.len())].to_string());
                self.suspend(terminal, app)?;
                print!("\r\n\x1b[1mlitek8s ui\x1b[0m: shell in {name}; exit the shell to come back.\r\n\r\n");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let r = exec::shell(&docker, &id).await;
                self.resume(terminal, app)?;
                match r {
                    Ok(code) => app.flash(format!("shell in {name} exited ({code})"), code != 0 && code != 130),
                    Err(e) => app.flash(format!("shell in {name}: {e:#}"), true),
                }
            }
            Effect::EditConfig => {
                let path = crate::dev::config::path();
                if !path.exists() {
                    let r = app.config.save_to(&path);
                    if let Err(e) = r {
                        app.flash(format!("{e:#}"), true);
                        return Ok(false);
                    }
                }
                let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
                self.suspend(terminal, app)?;
                let _ = terminal::disable_raw_mode();
                let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status();
                let _ = terminal::enable_raw_mode();
                self.resume(terminal, app)?;
                match status {
                    Ok(s) if s.success() => match crate::dev::config::DevConfig::load_from(&path) {
                        Ok(c) => {
                            app.apply(Msg::Config(Ok(c)));
                            app.flash("config file saved; it is valid", false);
                        }
                        Err(e) => {
                            app.config_error = Some(format!("{e:#}"));
                            app.flash("the config file has errors: fix them (enter on Config file to edit again)", true);
                        }
                    },
                    Ok(s) => app.flash(format!("{editor} exited with {s}"), true),
                    Err(e) => app.flash(format!("cannot run {editor}: {e}"), true),
                }
            }
            Effect::SaveConfig(c) => {
                let path = crate::dev::config::path();
                match c.save_to(&path) {
                    Ok(()) => {
                        app.apply(Msg::Config(Ok(c)));
                        let restart = app.restart_needed();
                        if restart.is_empty() {
                            app.flash("settings saved", false);
                        } else {
                            app.flash(format!("saved; restart the dev cluster for {} (Home: d, u)", restart.join(", ")), false);
                        }
                    }
                    Err(e) => app.flash(format!("{e:#}"), true),
                }
            }
            Effect::DockerContext(on) => {
                let r = ctx::switch_docker(on, &crate::dev::cluster::Cluster { name: app.cluster.clone() });
                self.wake_home.notify_one();
                done(r);
            }
            Effect::KubeContext(on) => {
                let r = ctx::switch_kube(on, &crate::dev::cluster::Cluster { name: app.cluster.clone() }, app.home.running.as_ref().map(|s| s.port));
                self.wake_home.notify_one();
                done(r);
            }
            Effect::Dev(op) => self.dev(op),
            Effect::Scale { namespace, name, replicas } => {
                tokio::spawn(async move { done(kube::scale(&kube, &namespace, &name, replicas).await) });
            }
            Effect::RestartDeployment { namespace, name } => {
                tokio::spawn(async move { done(kube::restart(&kube, &namespace, &name).await) });
            }
            Effect::DeletePod { namespace, name } => {
                tokio::spawn(async move { done(kube::delete_pod(&kube, &namespace, &name).await) });
            }
            Effect::PortForward { namespace, pod, local, remote } => {
                let forwards = self.forwards.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let r = forwards.start(kube, namespace, pod, local, remote).await;
                    let _ = tx.send(Msg::Forwards(forwards.list()));
                    done(r);
                });
            }
            Effect::StopForwards => {
                self.forwards.stop_all();
                app.apply(Msg::Forwards(Vec::new()));
                app.flash("port-forwards stopped", false);
            }
        }
        Ok(false)
    }

    /// Show another local cluster: its Docker socket, its API (followed
    /// through Home's samples).
    fn use_cluster(&mut self, app: &mut App, name: &str) {
        let cluster = crate::dev::cluster::Cluster { name: name.to_string() };
        app.cluster = cluster.name.clone();
        ctx::set_active_cluster(name);
        *self.active.lock().unwrap() = cluster.name.clone();
        if !self.local {
            app.flash("this UI shows a fixed --docker-host / --context; restart it without them to switch clusters", true);
            return;
        }
        let socket = cluster.docker_socket();
        if self.docker.display != client::Endpoint::unix(socket.clone()).display {
            self.docker_task.abort();
            self.docker = client::Endpoint::unix(socket);
            app.docker_endpoint = self.docker.display.clone();
            app.apply(Msg::DockerDown(format!("connecting to {name}…")));
            self.docker_task = docker::spawn(self.docker.clone(), self.tx.clone(), self.refresh.clone());
        }
        // The Kubernetes side follows the new cluster's port.
        self.follow_dev = true;
        self.kube_port = Some(0);
        app.kube_context = cluster.context();
        app.home.running = None;
        self.follow_dev_cluster(app);
        self.wake_home.notify_one();
    }

    /// Point the Kubernetes side at the dev cluster's current port.
    fn follow_dev_cluster(&mut self, app: &mut App) {
        if !self.follow_dev {
            return;
        }
        let port = app.home.running.as_ref().map(|s| s.port);
        if port == self.kube_port {
            return;
        }
        self.kube_port = port;
        for t in self.kube_watches.drain(..) {
            t.abort();
        }
        match port {
            Some(p) => {
                self.kube = dev_endpoint(p);
                app.kube_endpoint = self.kube.display.clone();
                self.kube_watches = kube::spawn_watches(self.kube.clone(), self.tx.clone());
            }
            None => app.apply(Msg::KubeDown(DEV_DOWN.into())),
        }
    }

    /// Give the terminal to a shell or an editor.
    fn suspend(&mut self, terminal: &mut Term, app: &App) -> Result<()> {
        self.input.pause();
        if app.mouse {
            let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
        }
        execute!(std::io::stdout(), terminal::LeaveAlternateScreen, ratatui::crossterm::cursor::Show)?;
        terminal.show_cursor()?;
        Ok(())
    }

    fn resume(&mut self, terminal: &mut Term, app: &App) -> Result<()> {
        execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
        if app.mouse {
            let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
        }
        terminal.hide_cursor()?;
        // Clear and redraw everything. Not terminal.clear(): it asks the
        // terminal for the cursor position, and that read fails while the
        // dropped event stream's wake-up is still pending.
        let size = terminal.size()?;
        terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
        self.input.resume();
        Ok(())
    }

    /// `litek8s dev up|down|delete` as a child process, its output on Home
    /// and Clusters.
    fn dev(&mut self, op: DevOp) {
        let tx = self.tx.clone();
        let wake = self.wake_home.clone();
        let wake_clusters = self.wake_clusters.clone();
        let refresh = self.refresh.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let args = op.args();
            let verb = args[1..].join(" ");
            let exe = std::env::current_exe().unwrap_or_else(|_| "litek8s".into());
            let child = tokio::process::Command::new(exe)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(false)
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::DevFinished(Err(format!("cannot run litek8s dev {verb}: {e}"))));
                    return;
                }
            };
            let _ = tx.send(Msg::DevOutput(format!("$ litek8s dev {verb}")));
            async fn pump<R: tokio::io::AsyncRead + Unpin>(r: Option<R>, tx: UnboundedSender<Msg>) {
                if let Some(r) = r {
                    let mut lines = tokio::io::BufReader::new(r).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let _ = tx.send(Msg::DevOutput(docker::printable(l.as_bytes())));
                    }
                }
            }
            let (a, b) = (tokio::spawn(pump(child.stdout.take(), tx.clone())), tokio::spawn(pump(child.stderr.take(), tx.clone())));
            let status = child.wait().await;
            let _ = tokio::join!(a, b);
            let r = match status {
                Ok(s) if s.success() => Ok(format!("litek8s dev {verb}: done")),
                Ok(s) => Err(format!("litek8s dev {verb} failed ({s}); see the output on Home or Clusters")),
                Err(e) => Err(format!("litek8s dev {verb}: {e}")),
            };
            let _ = tx.send(Msg::DevFinished(r));
            wake.notify_one();
            wake_clusters.notify_one();
            refresh.notify_one();
        });
    }
}

#[cfg(test)]
mod tests;
