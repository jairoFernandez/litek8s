//! A shell in a container (`docker exec -it <id> sh`) over the Docker API,
//! while the UI is suspended: the terminal stays in raw mode, keystrokes go
//! to the exec and its output to the terminal, window size changes follow.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::client::Endpoint;

/// Prefer bash, as Docker Desktop's "Open in terminal" does.
const SHELL: &str = "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi";

pub async fn shell(ep: &Endpoint, id: &str) -> Result<i64> {
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm".into());
    let created = ep
        .json(
            Method::POST,
            &format!("/containers/{id}/exec"),
            Some(&json!({
                "AttachStdin": true, "AttachStdout": true, "AttachStderr": true, "Tty": true,
                "Cmd": ["/bin/sh", "-c", SHELL], "Env": [format!("TERM={term}")], "ConsoleSize": [rows, cols]
            })),
        )
        .await?;
    let exec = created["Id"].as_str().context("no exec id")?.to_string();
    let (_, io) = ep.upgrade(Method::POST, &format!("/exec/{exec}/start"), "tcp", &[], Some(&json!({"Detach": false, "Tty": true, "ConsoleSize": [rows, cols]}))).await?;
    let _ = ep.json(Method::POST, &format!("/exec/{exec}/resize?h={rows}&w={cols}"), None).await;
    let (mut from, mut to) = tokio::io::split(io);

    let mut keys = Keys::start();
    let input = async {
        while let Some(k) = keys.rx.recv().await {
            if to.write_all(&k).await.is_err() {
                break;
            }
            let _ = to.flush().await;
        }
    };
    let output = async {
        let mut buf = vec![0u8; 32 * 1024];
        let mut out = std::io::stdout();
        loop {
            match from.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = out.write_all(&buf[..n]);
                    let _ = out.flush();
                }
            }
        }
    };
    let resize = async {
        let Ok(mut winch) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) else { return std::future::pending().await };
        while winch.recv().await.is_some() {
            if let Ok((c, r)) = ratatui::crossterm::terminal::size() {
                let _ = ep.json(Method::POST, &format!("/exec/{exec}/resize?h={r}&w={c}"), None).await;
            }
        }
    };
    tokio::select! {
        _ = output => {}
        _ = input => {}
        _ = resize => {}
    }
    keys.stop().await;
    let inspect = ep.json(Method::GET, &format!("/exec/{exec}/json"), None).await?;
    match inspect["ExitCode"].as_i64() {
        Some(code) => Ok(code),
        None if inspect["Running"].as_bool() == Some(true) => bail!("the exec is still running"),
        None => Ok(0),
    }
}

/// Keystrokes while the UI is suspended: a thread polling stdin, so it can
/// stop without eating the first key the UI should get back.
pub struct Keys {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Keys {
    pub fn start() -> Keys {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let fd = std::io::stdin().as_raw_fd();
                let mut buf = [0u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                    // SAFETY: one valid pollfd.
                    let n = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if n <= 0 || pfd.revents & libc::POLLIN == 0 {
                        continue;
                    }
                    // SAFETY: reads into our buffer.
                    let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                    if r <= 0 || tx.send(buf[..r as usize].to_vec()).is_err() {
                        break;
                    }
                }
            })
        };
        Keys { rx, stop, thread: Some(thread) }
    }

    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        // The thread polls with a 100 ms timeout: it is gone right after.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
