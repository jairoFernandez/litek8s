//! `docker exec` (and health checks): `crun exec` with a process spec
//! derived from the container's own, run from the engine with pipes or a
//! pty.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use hyper::body::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::watch;

use super::engine::{Ctr, Engine, RUN_DIR};
use super::error::{ApiError, ApiResult};
use crate::kube::agent::crun::image::random_hex;
use crate::kube::agent::crun::{spec as cspec, sys};

pub struct ExecInst {
    pub id: String,
    pub container: String,
    pub config: Value,
    pub running: Mutex<bool>,
    pub exit_code: watch::Sender<Option<i64>>,
    pub pid: Mutex<i64>,
    pub pty: Mutex<Option<Arc<tokio::io::unix::AsyncFd<OwnedFd>>>>,
    pub size: Mutex<Option<(u16, u16)>>,
}

impl ExecInst {
    pub fn inspect(&self) -> Value {
        let c = &self.config;
        let cmd: Vec<Value> = c["Cmd"].as_array().cloned().unwrap_or_default();
        json!({
            "ID": self.id,
            "Running": *self.running.lock().unwrap(),
            "ExitCode": *self.exit_code.borrow(),
            "ProcessConfig": {
                "tty": c["Tty"].as_bool().unwrap_or(false),
                "entrypoint": cmd.first().cloned().unwrap_or(Value::Null),
                "arguments": cmd.get(1..).map(|a| a.to_vec()).unwrap_or_default(),
                "privileged": c["Privileged"].as_bool().unwrap_or(false),
                "user": c["User"].as_str().unwrap_or_default(),
            },
            "OpenStdin": c["AttachStdin"].as_bool().unwrap_or(false),
            "OpenStderr": c["AttachStderr"].as_bool().unwrap_or(false),
            "OpenStdout": c["AttachStdout"].as_bool().unwrap_or(false),
            "CanRemove": false,
            "ContainerID": self.container,
            "DetachKeys": c["DetachKeys"].as_str().unwrap_or_default(),
            "Pid": *self.pid.lock().unwrap(),
        })
    }
}

/// A running exec's streams.
pub struct ExecIo {
    /// (1 stdout, 2 stderr) chunks; with a terminal everything is 1.
    pub output: BoxStream<'static, (u8, Bytes)>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub tty: bool,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// The process spec for an exec in `ctr`.
fn process_for(engine: &Engine, ctr: &Ctr, config: &Value, tty: bool) -> Result<Value> {
    let dir = engine.container_dir(&ctr.id);
    let bundle: Value = serde_json::from_slice(&std::fs::read(dir.join("config.json")).context("container has no bundle")?)?;
    let mut process = bundle["process"].clone();
    let argv = strings(&config["Cmd"]);
    if argv.is_empty() {
        bail!("No exec command specified");
    }
    process["args"] = json!(argv);
    process["terminal"] = json!(tty);
    let mut env: Vec<String> = strings(&process["env"]);
    for kv in strings(&config["Env"]) {
        let key = kv.split('=').next().unwrap_or_default().to_string();
        env.retain(|e| e.split('=').next() != Some(key.as_str()));
        env.push(kv);
    }
    process["env"] = json!(env);
    if let Some(wd) = config["WorkingDir"].as_str().filter(|w| w.starts_with('/')) {
        process["cwd"] = json!(wd);
    }
    if let Some(user) = config["User"].as_str().filter(|u| !u.is_empty()) {
        let pid = ctr.record().state.pid;
        let root = PathBuf::from(format!("/proc/{pid}/root"));
        let read = |f: &str| std::fs::read_to_string(super::engine::secure_join(&root, &format!("/etc/{f}"))).unwrap_or_default();
        let u = cspec::resolve_user(user, &read("passwd"), &read("group"))?;
        process["user"] = json!({"uid": u.uid, "gid": u.gid});
        if !u.additional_gids.is_empty() {
            process["user"]["additionalGids"] = json!(u.additional_gids);
        }
        let home = format!("HOME={}", u.home);
        let mut env: Vec<String> = strings(&process["env"]);
        env.retain(|e| !e.starts_with("HOME="));
        env.push(home);
        if !strings(&config["Env"]).iter().any(|e| e.starts_with("HOME=")) {
            process["env"] = json!(env);
        }
    }
    if let Some(o) = process.as_object_mut() {
        o.remove("consoleSize");
    }
    Ok(process)
}

fn read_stream<R: AsyncRead + Unpin + Send + 'static>(reader: R, channel: u8) -> BoxStream<'static, (u8, Bytes)> {
    futures_util::stream::unfold(Some(reader), move |state| async move {
        let mut r = state?;
        let mut buf = vec![0u8; 32 * 1024];
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                buf.truncate(n);
                Some(((channel, Bytes::from(buf)), Some(r)))
            }
        }
    })
    .boxed()
}

struct PtyWriter(Arc<tokio::io::unix::AsyncFd<OwnedFd>>);

impl AsyncWrite for PtyWriter {
    fn poll_write(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        use std::os::fd::AsRawFd;
        loop {
            let mut guard = std::task::ready!(self.0.poll_write_ready(cx))?;
            // SAFETY: writing from the caller's buffer to our fd.
            match guard.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                Ok(result) => return std::task::Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        use std::os::fd::AsRawFd;
        // SAFETY: one byte (^D, end of input on a terminal).
        unsafe { libc::write(self.0.as_raw_fd(), [4u8].as_ptr().cast(), 1) };
        std::task::Poll::Ready(Ok(()))
    }
}

fn pty_output(fd: Arc<tokio::io::unix::AsyncFd<OwnedFd>>) -> BoxStream<'static, (u8, Bytes)> {
    use std::os::fd::AsRawFd;
    futures_util::stream::unfold(fd, |fd| async move {
        loop {
            let mut guard = fd.readable().await.ok()?;
            let mut buf = vec![0u8; 32 * 1024];
            // SAFETY: reading into our buffer.
            match guard.try_io(|f| {
                let n = unsafe { libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                Ok(Ok(0)) | Ok(Err(_)) => return None,
                Ok(Ok(n)) => {
                    buf.truncate(n);
                    return Some(((1u8, Bytes::from(buf)), fd));
                }
                Err(_would_block) => continue,
            }
        }
    })
    .boxed()
}

fn set_nonblocking(fd: &OwnedFd) {
    use std::os::fd::AsRawFd;
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

impl Engine {
    pub fn create_exec(&self, key: &str, body: &Value) -> ApiResult<String> {
        let ctr = self.get(key)?;
        let rec = ctr.record();
        if rec.state.status != "running" {
            return Err(ApiError::conflict(format!("container {} is not running", rec.id)));
        }
        if strings(&body["Cmd"]).is_empty() {
            return Err(ApiError::bad_request("No exec command specified"));
        }
        let id = random_hex(32);
        let inst = Arc::new(ExecInst {
            id: id.clone(),
            container: rec.id.clone(),
            config: body.clone(),
            running: Mutex::new(false),
            exit_code: watch::channel(None).0,
            pid: Mutex::new(0),
            pty: Mutex::new(None),
            size: Mutex::new(body["ConsoleSize"].as_array().and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16)))),
        });
        self.execs.lock().unwrap().insert(id.clone(), inst);
        let cmd = strings(&body["Cmd"]).join(" ");
        self.emit_container(&rec, &format!("exec_create: {cmd}"), &[("execID", id.clone())]);
        Ok(id)
    }

    pub fn find_exec(&self, id: &str) -> ApiResult<Arc<ExecInst>> {
        self.execs.lock().unwrap().get(id).cloned().ok_or_else(|| ApiError::not_found(format!("No such exec instance: {id}")))
    }

    pub fn resize_exec(&self, id: &str, rows: u16, cols: u16) -> ApiResult<()> {
        use std::os::fd::AsRawFd;
        let inst = self.find_exec(id)?;
        *inst.size.lock().unwrap() = Some((rows, cols));
        if let Some(pty) = inst.pty.lock().unwrap().as_ref() {
            let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: TIOCSWINSZ reads the winsize we pass.
            unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        }
        Ok(())
    }

    /// Start an exec; with `detach`, its output is discarded.
    pub async fn start_exec(self: &Arc<Self>, id: &str, detach: bool, tty_override: Option<bool>) -> ApiResult<ExecIo> {
        let inst = self.find_exec(id)?;
        {
            let mut running = inst.running.lock().unwrap();
            if *running || inst.exit_code.borrow().is_some() {
                return Err(ApiError::conflict(format!("exec {id} has already been started")));
            }
            *running = true;
        }
        let ctr = self.get(&inst.container)?;
        if ctr.record().state.status != "running" {
            *inst.running.lock().unwrap() = false;
            return Err(ApiError::conflict(format!("container {} is not running", inst.container)));
        }
        let tty = tty_override.unwrap_or_else(|| inst.config["Tty"].as_bool().unwrap_or(false)) && !detach;
        let stdin = inst.config["AttachStdin"].as_bool().unwrap_or(false) && !detach;
        let process = match process_for(self, &ctr, &inst.config, tty) {
            Ok(p) => p,
            Err(e) => {
                *inst.running.lock().unwrap() = false;
                let _ = inst.exit_code.send(Some(126));
                return Err(ApiError::new(500, format!("{e:#}")));
            }
        };
        let rec = ctr.record();
        let dir = self.container_dir(&ctr.id);
        let process_file = dir.join(format!("exec-{}.json", &id[..12]));
        std::fs::write(&process_file, serde_json::to_vec(&process)?)?;
        let pid_file = dir.join(format!("exec-{}.pid", &id[..12]));
        let mut cmd = tokio::process::Command::new(&self.crun);
        cmd.arg("--root").arg(&self.crun_root).arg("exec").arg("--process").arg(&process_file).arg("--pid-file").arg(&pid_file);
        let console = PathBuf::from(RUN_DIR).join(format!("x{}.sock", &id[..12]));
        let listener = if tty {
            let _ = std::fs::remove_file(&console);
            let l = std::os::unix::net::UnixListener::bind(&console).map_err(|e| ApiError::new(500, format!("console socket: {e}")))?;
            cmd.arg("--console-socket").arg(&console);
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            Some(l)
        } else if detach {
            cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            None
        } else {
            cmd.stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() }).stdout(std::process::Stdio::piped());
            None
        };
        cmd.stderr(if detach && !tty { std::process::Stdio::null() } else { std::process::Stdio::piped() }).arg(&ctr.id);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                *inst.running.lock().unwrap() = false;
                return Err(ApiError::new(500, format!("failed to run crun exec: {e}")));
            }
        };
        let (output, input): (BoxStream<'static, (u8, Bytes)>, Pin<Box<dyn AsyncWrite + Send>>) = match listener {
            Some(l) => {
                let fd = match tokio::task::spawn_blocking(move || sys::receive_fd(&l, 10_000)).await {
                    Ok(Ok(fd)) => fd,
                    other => {
                        let _ = child.kill().await;
                        let _ = std::fs::remove_file(&console);
                        *inst.running.lock().unwrap() = false;
                        let msg = match other {
                            Ok(Err(e)) => e.to_string(),
                            _ => "no terminal".into(),
                        };
                        let mut err = String::new();
                        if let Some(mut e) = child.stderr.take() {
                            let _ = e.read_to_string(&mut err).await;
                        }
                        return Err(ApiError::new(500, format!("exec failed: {msg} {}", err.trim())));
                    }
                };
                let _ = std::fs::remove_file(&console);
                set_nonblocking(&fd);
                let afd = Arc::new(tokio::io::unix::AsyncFd::new(fd)?);
                if let Some((rows, cols)) = *inst.size.lock().unwrap() {
                    use std::os::fd::AsRawFd;
                    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                    // SAFETY: TIOCSWINSZ reads the winsize we pass.
                    unsafe { libc::ioctl(afd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
                }
                *inst.pty.lock().unwrap() = Some(afd.clone());
                let mut out = pty_output(afd.clone());
                if let Some(e) = child.stderr.take() {
                    out = futures_util::stream::select(out, read_stream(e, 2)).boxed();
                }
                (out, Box::pin(PtyWriter(afd)))
            }
            None => {
                let mut streams = Vec::new();
                if let Some(o) = child.stdout.take() {
                    streams.push(read_stream(o, 1));
                }
                if let Some(e) = child.stderr.take() {
                    streams.push(read_stream(e, 2));
                }
                let out = futures_util::stream::select_all(streams).boxed();
                let input: Pin<Box<dyn AsyncWrite + Send>> = match child.stdin.take() {
                    Some(w) => Box::pin(w),
                    None => Box::pin(tokio::io::sink()),
                };
                (out, input)
            }
        };
        self.emit_container(&rec, &format!("exec_start: {}", strings(&inst.config["Cmd"]).join(" ")), &[("execID", id.to_string())]);
        let engine = self.clone();
        let inst2 = inst.clone();
        tokio::spawn(async move {
            // The pid, once crun wrote it.
            for _ in 0..50 {
                if let Some(p) = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i64>().ok()) {
                    *inst2.pid.lock().unwrap() = p;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let code = child.wait().await.ok().and_then(|s| s.code()).map(i64::from).unwrap_or(-1);
            *inst2.running.lock().unwrap() = false;
            let _ = inst2.exit_code.send(Some(code));
            let _ = std::fs::remove_file(&process_file);
            let _ = std::fs::remove_file(&pid_file);
            if let Some(c) = engine.find(&inst2.container) {
                let rec = c.record();
                engine.emit_container(&rec, "exec_die", &[("execID", inst2.id.clone()), ("exitCode", code.to_string())]);
            }
            // Keep it for inspect a while, then forget it.
            tokio::time::sleep(Duration::from_secs(300)).await;
            engine.execs.lock().unwrap().remove(&inst2.id);
        });
        Ok(ExecIo { output, input, tty })
    }
}

/// Run `argv` in the container and collect its exit code and (the start
/// of) its output: health checks.
pub async fn run_capture(engine: &Arc<Engine>, ctr: &Arc<Ctr>, argv: Vec<String>, timeout: Duration) -> Result<(i64, String)> {
    let config = json!({"Cmd": argv});
    let process = process_for(engine, ctr, &config, false)?;
    let dir = engine.container_dir(&ctr.id);
    let tag = random_hex(6);
    let process_file = dir.join(format!("health-{tag}.json"));
    let pid_file = dir.join(format!("health-{tag}.pid"));
    std::fs::write(&process_file, serde_json::to_vec(&process)?)?;
    let mut child = tokio::process::Command::new(&engine.crun)
        .arg("--root")
        .arg(&engine.crun_root)
        .arg("exec")
        .arg("--process")
        .arg(&process_file)
        .arg("--pid-file")
        .arg(&pid_file)
        .arg(&ctr.id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let collect = async {
        let mut buf = Vec::new();
        let mut ebuf = Vec::new();
        let _ = tokio::join!(out.read_to_end(&mut buf), err.read_to_end(&mut ebuf));
        buf.extend(ebuf);
        let status = child.wait().await;
        (status, buf)
    };
    let result = tokio::time::timeout(timeout, collect).await;
    let _ = std::fs::remove_file(&process_file);
    let r = match result {
        Ok((status, buf)) => {
            let code = status.ok().and_then(|s| s.code()).map(i64::from).unwrap_or(-1);
            let mut text = String::from_utf8_lossy(&buf[..buf.len().min(4096)]).to_string();
            if text.is_empty() && code != 0 {
                text = String::new();
            }
            (code, text)
        }
        Err(_) => {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                // SAFETY: plain signal.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            (-1, format!("Health check exceeded timeout ({}s)", timeout.as_secs()))
        }
    };
    let _ = std::fs::remove_file(&pid_file);
    let _: BTreeMap<String, String> = BTreeMap::new();
    Ok(r)
}
