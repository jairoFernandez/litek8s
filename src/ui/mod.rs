//! `litek8s ui`: a terminal UI for the local dev environment, what
//! OrbStack's and Docker Desktop's windows give their users: the dev
//! cluster's state and resources, Docker containers (grouped by compose
//! project) with logs, shells and actions, images and volumes, a free-space
//! view, settings, and a light Kubernetes view. It works over SSH and talks
//! only to litek8s's public APIs (the Docker socket, the Kubernetes API,
//! the dev supervisor's state and CLI), so it shows a remote litek8s the
//! same way. See docs/ui.md.

pub mod app;
pub(crate) mod client;
mod clusters;
mod ctx;
pub(crate) mod docker;
mod exec;
mod home;
mod kube;
mod obs;

/// The API server a kubeconfig context (or the local dev cluster) names,
/// for the CLI (`litek8s logs`, `alerts`, `query`).
pub(crate) use kube::resolve as resolve_kube;
// What `litek8s mcp` reuses of the Kubernetes view.
pub(crate) use kube::{auth as kube_auth, ops as kube_ops, resources as kube_resources, state as kube_state};
pub mod theme;
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
    // The context's own namespace unless -n says otherwise.
    let start_ns = args.namespace.clone().or_else(|| kube::auth::load().ok().and_then(|c| c.namespace(&app.kube_context)));
    app.kube = kube::KubeUi::new(start_ns.clone());
    app.cluster = cluster.name.clone();
    app.apply(Msg::Config(crate::dev::config::DevConfig::load().map_err(|e| format!("{e:#}"))));

    let refresh = Arc::new(Notify::new());
    let pace = Arc::new(AtomicU64::new(home::VISIBLE));
    let wake_home = Arc::new(Notify::new());
    let active = Arc::new(Mutex::new(cluster.name.clone()));
    let clusters_visible = Arc::new(AtomicBool::new(false));
    let wake_clusters = Arc::new(Notify::new());
    let obs_visible = Arc::new(AtomicBool::new(false));
    let wake_obs = Arc::new(Notify::new());
    let obs_ep: obs::Shared = Arc::new(Mutex::new(fixed.as_ref().map(|(ep, _)| ep.clone())));
    let docker_task = docker::spawn(docker_ep.clone(), tx.clone(), refresh.clone());
    let tasks = vec![
        home::spawn(tx.clone(), local, pace.clone(), wake_home.clone(), active.clone(), fixed.as_ref().filter(|_| !local).map(|(ep, _)| ep.clone())),
        clusters::spawn(tx.clone(), clusters_visible.clone(), wake_clusters.clone()),
        obs::spawn(tx.clone(), obs_visible.clone(), wake_obs.clone(), obs_ep.clone()),
    ];
    let kube_watches = if fixed.is_some() { kube::spawn_watches(kube_ep.clone(), tx.clone()) } else { Vec::new() };
    if fixed.is_none() {
        let _ = tx.send(Msg::KubeDown(DEV_DOWN.into()));
    }

    if fixed.is_some() {
        app.kube.reset(start_ns);
    }
    // The look: ui.yaml's theme in this terminal's colors, its background
    // asked once before the input reader starts (see probe_background).
    let ui_config = kube::uiconfig::load().unwrap_or_default();
    let mut term = theme::Term::from_env();
    let mut terminal = setup(args.mouse)?;
    let probe_started = Instant::now();
    let mut probed = None;
    if ui_config.background == theme::Background::Auto && ui_config.theme == theme::Preset::Default && matches!(term.depth, theme::Depth::True | theme::Depth::X256) && !term.ascii {
        if let Some((r, g, b)) = probe_background(Duration::from_millis(150)) {
            term.bg = Some(theme::Bg::Rgb(r, g, b));
        }
        probed = Some(probe_started.elapsed());
    }
    theme::init(term);
    app.kube.theme = theme::Theme::from_config(&ui_config);
    let input = Input::new();
    let kube_rt = kube::rt::Rt::new(kube_ep.clone(), tx.clone());
    let mut rt = Runtime {
        docker: docker_ep,
        docker_task,
        local,
        active,
        clusters_visible,
        wake_clusters,
        obs_visible,
        wake_obs,
        obs_ep,
        kube: kube_ep,
        follow_dev: fixed.is_none(),
        kube_port: None,
        kube_watches,
        tx,
        refresh,
        wake_home,
        kube_rt,
        log_task: None,
        input,
        stats: Stats { started, probe: probed, first_frame: None, data_frame: None, containers_seen: false, frames: 0, kube_frame: None, key_at: None, keys: 0, key_total: Duration::ZERO, key_max: Duration::ZERO },
    };
    rt.kube_rt.load_settings();
    let result = rt.event_loop(&mut terminal, &mut app, rx, &pace).await;
    rt.kube_rt.stop_all();
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
            "litek8s ui: background asked in {}, first frame {}, first frame with containers {}, first Kubernetes list {}, {} frames in {:.1} s",
            ms(s.probe),
            ms(s.first_frame),
            ms(s.data_frame),
            ms(s.kube_frame),
            s.frames,
            s.started.elapsed().as_secs_f64()
        );
        if s.keys > 0 {
            eprintln!("litek8s ui: key to frame: {} keys, {} on average, {} at most", s.keys, ms(Some(s.key_total / s.keys as u32)), ms(Some(s.key_max)));
        }
    }
    result
}

/// How often ages ("5m") are redrawn when nothing else changes.
const AGES_EVERY: Duration = Duration::from_secs(30);

/// `LITEK8S_UI_STATS=1`: what the UI did, printed when it exits.
struct Stats {
    started: Instant,
    /// Asking the terminal for its background.
    probe: Option<Duration>,
    first_frame: Option<Duration>,
    /// The first frame showing the engine's containers.
    data_frame: Option<Duration>,
    containers_seen: bool,
    frames: u64,
    /// The first frame of the Kubernetes view with its list.
    kube_frame: Option<Duration>,
    /// Keys to the frame they changed (filtering, moving).
    key_at: Option<Instant>,
    keys: u64,
    key_total: Duration,
    key_max: Duration,
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

/// The terminal's background color, asked with OSC 11 and read straight
/// from the terminal before crossterm's reader starts (crossterm would
/// take the answer for keys). A DA1 query follows: terminals answer in
/// order and practically all answer DA1, so the wait ends with that answer
/// (a millisecond or two locally, a round trip over ssh); one that answers
/// neither costs `timeout`, once, and the UI starts with colors that read
/// on both backgrounds. Unlike the cursor-position query that once failed
/// the start in terminals that never answer, nothing here can fail.
#[cfg(unix)]
fn probe_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    out.flush().ok()?;
    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reads at most chunk.len() bytes into chunk.
        let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..r as usize]);
        if da1_answered(&buf) || buf.len() > 4096 {
            break;
        }
    }
    parse_osc11(&buf)
}

#[cfg(not(unix))]
fn probe_background(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// `ESC [ ? … c`: the terminal's primary device attributes.
fn da1_answered(buf: &[u8]) -> bool {
    let mut from = 0;
    while let Some(p) = buf[from..].windows(3).position(|w| w == b"\x1b[?") {
        let tail = &buf[from + p + 3..];
        let n = tail.iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
        if tail.get(n) == Some(&b'c') {
            return true;
        }
        from += p + 3;
    }
    false
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (1 to 4 hex digits a component), ended
/// by BEL or ST.
fn parse_osc11(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let s = String::from_utf8_lossy(buf);
    let at = s.find("]11;")?;
    let rest = &s[at + 4..];
    let rest = rest.strip_prefix("rgba:").or_else(|| rest.strip_prefix("rgb:"))?;
    let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let mut parts = rest[..end].split('/');
    let mut comp = || -> Option<u8> {
        let h = parts.next()?;
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    };
    Some((comp()?, comp()?, comp()?))
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
    /// The Observe view samples only while shown.
    obs_visible: Arc<AtomicBool>,
    wake_obs: Arc<Notify>,
    obs_ep: obs::Shared,
    kube: Endpoint,
    /// The Kubernetes API is the local dev cluster's, wherever it runs.
    follow_dev: bool,
    kube_port: Option<u16>,
    kube_watches: Vec<tokio::task::JoinHandle<()>>,
    tx: UnboundedSender<Msg>,
    refresh: Arc<Notify>,
    wake_home: Arc<Notify>,
    kube_rt: kube::rt::Rt,
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
            // What the Kubernetes view asked for on its own (informers for
            // a new view or cluster, a container picked).
            loop {
                let more = app.kube.take_effects();
                if more.is_empty() {
                    break;
                }
                dirty = true;
                for e in more {
                    self.effect(Effect::Kube(e), terminal, app).await?;
                }
            }
            if let Some((text, error)) = app.kube.flash.take() {
                app.flash(text, error);
            }
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
                if self.stats.kube_frame.is_none() && app.tab == Tab::Kube && app.kube.top_table().is_some_and(|t| t.synced && t.total > 0) {
                    self.stats.kube_frame = Some(self.stats.started.elapsed());
                }
                if let Some(at) = self.stats.key_at.take() {
                    let d = at.elapsed();
                    self.stats.keys += 1;
                    self.stats.key_total += d;
                    self.stats.key_max = self.stats.key_max.max(d);
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
                        self.stats.key_at = Some(Instant::now());
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
            let on_obs = app.tab == Tab::Observe && app.logs.is_none();
            if self.obs_visible.swap(on_obs, Ordering::Relaxed) != on_obs && on_obs {
                self.wake_obs.notify_one();
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
            Msg::Obs(_) => app.tab == Tab::Observe && app.logs.is_none(),
            Msg::LogLines(..) | Msg::LogEnd(..) => app.logs.is_some(),
            Msg::Kube(kube::KMsg::Data(..) | kube::KMsg::LogLines { .. } | kube::KMsg::Ext { .. } | kube::KMsg::Forwards(_)) => app.tab == Tab::Kube && app.logs.is_none(),
            _ => true,
        }
    }

    /// Carry out one effect; `true` to quit.
    async fn effect(&mut self, e: Effect, terminal: &mut Term, app: &mut App) -> Result<bool> {
        let docker = self.docker.clone();
        let tx = self.tx.clone();
        let refresh = self.refresh.clone();
        let done = move |r: Result<String>| {
            let _ = tx.send(Msg::Done(r.map_err(|e| format!("{e:#}"))));
        };
        match e {
            Effect::Quit => return Ok(true),
            Effect::Refresh => {
                self.wake_obs.notify_one();
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
                    *self.obs_ep.lock().unwrap() = Some(ep.clone());
                    self.kube = ep;
                    app.kube_endpoint = self.kube.display.clone();
                    let ns = kube::auth::load().ok().and_then(|c| c.namespace(&ctx));
                    app.kube_context = ctx;
                    self.kube_watches = kube::spawn_watches(self.kube.clone(), self.tx.clone());
                    self.kube_rt.reset(self.kube.clone());
                    app.kube.reset(ns);
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
                    LogSource::Search { namespace, pod, grep } => tokio::spawn(obs::search(self.kube.clone(), namespace, pod, grep, id, tx)),
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
            Effect::AckAlerts(alerts) => {
                let wake = self.wake_obs.clone();
                let kube = self.kube.clone();
                tokio::spawn(async move {
                    done(obs::ack(&kube, &alerts).await);
                    wake.notify_one();
                });
            }
            Effect::Kube(e) => kube::rt::effect(self, e, terminal, app).await?,
        }
        Ok(false)
    }

    /// Show another local cluster: its Docker socket, its API (followed
    /// through Home's samples).
    fn use_cluster(&mut self, app: &mut App, name: &str) {
        let cluster = crate::dev::cluster::Cluster { name: name.to_string() };
        if app.cluster != cluster.name {
            app.history.clear();
        }
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
                *self.obs_ep.lock().unwrap() = Some(self.kube.clone());
                app.kube_endpoint = self.kube.display.clone();
                self.kube_watches = kube::spawn_watches(self.kube.clone(), self.tx.clone());
                self.kube_rt.reset(self.kube.clone());
                let ns = app.kube.namespace.clone();
                app.kube.reset(ns);
            }
            None => {
                *self.obs_ep.lock().unwrap() = None;
                app.apply(Msg::KubeDown(DEV_DOWN.into()));
            }
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

#[cfg(test)]
mod probe_tests {
    #[test]
    fn background_answers() {
        assert_eq!(super::parse_osc11(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[?62;22c"), Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(super::parse_osc11(b"\x1b]11;rgb:ff/ff/ff\x07"), Some((255, 255, 255)));
        assert_eq!(super::parse_osc11(b"\x1b]11;rgb:f/0/8\x07"), Some((255, 0, 136)));
        assert_eq!(super::parse_osc11(b"\x1b[?1;2c"), None);
        assert!(super::da1_answered(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;22c"));
        assert!(super::da1_answered(b"\x1b[?6c"));
        assert!(!super::da1_answered(b"\x1b[?62;2"));
    }
}
