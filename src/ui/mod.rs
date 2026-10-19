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
mod ctx;
mod docker;
mod exec;
mod home;
mod kube;
pub mod model;
pub mod view;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
    /// Docker API to show (default: litek8s's socket, ~/.litek8s/docker.sock)
    #[arg(long, value_name = "URL")]
    pub docker_host: Option<String>,
    /// kubeconfig context to show (default: the local dev cluster)
    #[arg(long)]
    pub context: Option<String>,
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
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("litek8s ui needs a terminal");
    }
    let socket = crate::docker::host_socket_path();
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
        None => (dev_endpoint(crate::dev::DEFAULT_PORT), crate::dev::kubeconfig::CONTEXT.to_string()),
    };

    let (tx, rx) = unbounded_channel();
    let mut app = App::new(docker_ep.display.clone(), kube_ep.display.clone(), kube_context);
    app.mouse = args.mouse;
    app.namespace = args.namespace.clone();
    app.apply(Msg::Config(crate::dev::config::DevConfig::load().map_err(|e| format!("{e:#}"))));

    let refresh = Arc::new(Notify::new());
    let fast = Arc::new(AtomicBool::new(true));
    let wake_home = Arc::new(Notify::new());
    let tasks = vec![
        docker::spawn(docker_ep.clone(), tx.clone(), refresh.clone()),
        home::spawn(tx.clone(), local, fast.clone(), wake_home.clone()),
    ];
    let kube_watches = if fixed.is_some() { kube::spawn_watches(kube_ep.clone(), tx.clone()) } else { Vec::new() };
    if fixed.is_none() {
        let _ = tx.send(Msg::KubeDown(DEV_DOWN.into()));
    }

    let mut terminal = setup(args.mouse)?;
    let input = Input::spawn();
    let mut rt = Runtime {
        docker: docker_ep,
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
    };
    let result = rt.event_loop(&mut terminal, &mut app, rx, &fast).await;
    rt.forwards.stop_all();
    if let Some(t) = rt.log_task.take() {
        t.abort();
    }
    for t in tasks.into_iter().chain(rt.kube_watches.drain(..)) {
        t.abort();
    }
    restore(args.mouse);
    result
}

const DEV_DOWN: &str = "the dev cluster is not running (Home: u starts it)";

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
    let mut t = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    t.hide_cursor()?;
    t.clear()?;
    Ok(t)
}

fn restore(mouse: bool) {
    if mouse {
        let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    }
    let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// Terminal events from a thread that can be paused while a shell or an
/// editor owns the terminal (so it does not steal their keystrokes).
struct Input {
    rx: UnboundedReceiver<Event>,
    ctl: Arc<(Mutex<InputState>, Condvar)>,
}

#[derive(PartialEq)]
enum InputState {
    Running,
    PauseRequested,
    Paused,
}

impl Input {
    fn spawn() -> Input {
        let (tx, rx) = unbounded_channel();
        let ctl = Arc::new((Mutex::new(InputState::Running), Condvar::new()));
        let c = ctl.clone();
        std::thread::spawn(move || {
            let (lock, cvar) = &*c;
            loop {
                {
                    let mut st = lock.lock().unwrap();
                    if *st == InputState::PauseRequested {
                        *st = InputState::Paused;
                        cvar.notify_all();
                    }
                    while *st == InputState::Paused {
                        st = cvar.wait(st).unwrap();
                    }
                }
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        Input { rx, ctl }
    }

    fn pause(&self) {
        let (lock, cvar) = &*self.ctl;
        let mut st = lock.lock().unwrap();
        *st = InputState::PauseRequested;
        while *st != InputState::Paused {
            st = cvar.wait(st).unwrap();
        }
    }

    fn resume(&mut self) {
        while self.rx.try_recv().is_ok() {}
        let (lock, cvar) = &*self.ctl;
        *lock.lock().unwrap() = InputState::Running;
        cvar.notify_all();
    }
}

struct Runtime {
    docker: Endpoint,
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
}

impl Runtime {
    async fn event_loop(&mut self, terminal: &mut Term, app: &mut App, mut rx: UnboundedReceiver<Msg>, fast: &AtomicBool) -> Result<()> {
        let mut dirty = true;
        let mut status_at: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            if dirty {
                app.now = chrono::Utc::now().timestamp();
                terminal.draw(|f| view::draw(f, app))?;
                dirty = false;
            }
            let effects = tokio::select! {
                ev = self.input.rx.recv() => match ev {
                    Some(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if app.status.is_some() && app.overlay == app::Overlay::None {
                            app.status = None;
                        }
                        app.key(k)
                    }
                    Some(Event::Mouse(m)) => app.mouse(m),
                    Some(Event::Resize(..)) => Vec::new(),
                    Some(_) => continue,
                    None => return Ok(()),
                },
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    let was_status = app.status.clone();
                    app.apply(msg);
                    // Take what else is queued before drawing (log bursts).
                    while let Ok(m) = rx.try_recv() {
                        app.apply(m);
                    }
                    if app.status != was_status && app.status.is_some() {
                        status_at = Some(Instant::now());
                    }
                    self.follow_dev_cluster(app);
                    Vec::new()
                }
                _ = tick.tick() => {
                    if status_at.is_some_and(|t| t.elapsed() > Duration::from_secs(8)) {
                        app.status = None;
                        status_at = None;
                    }
                    Vec::new()
                }
            };
            dirty = true;
            fast.store(matches!(app.tab, Tab::Home | Tab::Space), Ordering::Relaxed);
            for e in effects {
                if self.effect(e, terminal, app).await? {
                    return Ok(());
                }
            }
            if app.status.is_some() && status_at.is_none() {
                status_at = Some(Instant::now());
            }
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
                let r = ctx::switch_docker(on);
                self.wake_home.notify_one();
                done(r);
            }
            Effect::KubeContext(on) => {
                let r = ctx::switch_kube(on, app.home.running.as_ref().map(|s| s.port));
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
        terminal.clear()?;
        self.input.resume();
        Ok(())
    }

    /// `litek8s dev up|down|reset` as a child process, its output on Home.
    fn dev(&mut self, op: DevOp) {
        let tx = self.tx.clone();
        let wake = self.wake_home.clone();
        let refresh = self.refresh.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let verb = match op {
                DevOp::Up => "up",
                DevOp::Down => "down",
                DevOp::Reset => "reset",
            };
            let exe = std::env::current_exe().unwrap_or_else(|_| "litek8s".into());
            let child = tokio::process::Command::new(exe)
                .args(["dev", verb])
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
                Ok(s) => Err(format!("litek8s dev {verb} failed ({s}); see the output on Home")),
                Err(e) => Err(format!("litek8s dev {verb}: {e}")),
            };
            let _ = tx.send(Msg::DevFinished(r));
            wake.notify_one();
            refresh.notify_one();
        });
    }
}

#[cfg(test)]
mod tests;
