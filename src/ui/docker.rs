//! The Docker Engine API side: containers, images and volumes kept current
//! from the engine's event stream (no polling), actions, and logs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hyper::Method;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::{ContainerOp, Msg};
use super::client::{Endpoint, Lines, enc};
use super::model;

/// Lists everything, then re-lists after engine events; `refresh` forces
/// a re-list.
pub fn spawn(ep: Endpoint, tx: UnboundedSender<Msg>, refresh: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match ep.json(Method::GET, "/info", None).await {
                Ok(info) => {
                    backoff = Duration::from_secs(1);
                    let version = ep.json(Method::GET, "/version", None).await.unwrap_or(Value::Null);
                    let _ = tx.send(Msg::DockerInfo {
                        version: format!("API {}", version["Version"].as_str().or(info["ServerVersion"].as_str()).unwrap_or("?")),
                        cpus: info["NCPU"].as_u64(),
                        memory: info["MemTotal"].as_u64(),
                        os: info["Architecture"].as_str().unwrap_or_default().to_string(),
                    });
                }
                Err(e) => {
                    if tx.send(Msg::DockerDown(format!("{e:#}"))).is_err() {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = refresh.notified() => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                    continue;
                }
            }
            // Subscribe before listing so nothing between the two is missed.
            let filters = enc(r#"{"type":["container","image","volume"]}"#);
            let events = ep.send(Method::GET, &format!("/events?filters={filters}"), None, Vec::new()).await;
            if list_all(&ep, &tx, true).await.is_err() {
                continue;
            }
            let Ok(events) = events else { continue };
            let mut lines = Lines::new(events.into_body());
            // Re-list shortly after a burst of events; sizes (system df walks
            // directories) at most every 2 s, and only after events that
            // change them.
            let mut list_at: Option<Instant> = None;
            let mut sizes_at: Option<Instant> = None;
            let mut last_sizes = Instant::now();
            loop {
                let next = [list_at, sizes_at].into_iter().flatten().min();
                tokio::select! {
                    line = lines.next() => match line {
                        Some(l) => {
                            let soon = Instant::now() + Duration::from_millis(150);
                            list_at.get_or_insert(soon);
                            if let Ok(ev) = serde_json::from_slice::<Value>(&l) {
                                let a = ev["Action"].as_str().unwrap_or_default();
                                if matches!(a, "destroy" | "delete" | "create" | "die" | "pull" | "tag" | "untag" | "prune" | "load" | "import" | "mount" | "unmount") {
                                    sizes_at.get_or_insert(soon.max(last_sizes + Duration::from_secs(2)));
                                }
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now).into()), if next.is_some() => {
                        let now = Instant::now();
                        if list_at.is_some_and(|t| t <= now) {
                            list_at = None;
                            if list_all(&ep, &tx, false).await.is_err() {
                                break;
                            }
                        }
                        if sizes_at.is_some_and(|t| t <= now) {
                            sizes_at = None;
                            last_sizes = now;
                            sizes(&ep, &tx).await;
                        }
                    }
                    _ = refresh.notified() => {
                        let _ = list_all(&ep, &tx, true).await;
                        last_sizes = Instant::now();
                    }
                }
            }
        }
    })
}

async fn list_all(ep: &Endpoint, tx: &UnboundedSender<Msg>, with_sizes: bool) -> Result<()> {
    let containers = ep.json(Method::GET, "/containers/json?all=1", None).await?;
    let _ = tx.send(Msg::Containers(model::containers(&containers)));
    let images = ep.json(Method::GET, "/images/json", None).await?;
    let _ = tx.send(Msg::Images(model::images(&images)));
    let volumes = ep.json(Method::GET, "/volumes", None).await?;
    let _ = tx.send(Msg::Volumes(model::volumes(&volumes)));
    if with_sizes {
        sizes(ep, tx).await;
    }
    Ok(())
}

async fn sizes(ep: &Endpoint, tx: &UnboundedSender<Msg>) {
    if let Ok(df) = ep.json(Method::GET, "/system/df", None).await {
        let _ = tx.send(Msg::Disk(model::disk_usage(&df)));
    }
}

pub async fn containers(ep: &Endpoint, op: &ContainerOp, ids: &[String]) -> Result<String> {
    let mut failed = Vec::new();
    for id in ids {
        let r = match op {
            ContainerOp::Start => ep.json(Method::POST, &format!("/containers/{id}/start"), None).await,
            ContainerOp::Stop => ep.json(Method::POST, &format!("/containers/{id}/stop"), None).await,
            ContainerOp::Restart => ep.json(Method::POST, &format!("/containers/{id}/restart"), None).await,
            ContainerOp::Remove => ep.json(Method::DELETE, &format!("/containers/{id}?force=1"), None).await,
        };
        if let Err(e) = r {
            failed.push(format!("{}: {e:#}", &id[..id.len().min(12)]));
        }
    }
    if !failed.is_empty() {
        bail!("{} failed: {}", op.verb(), failed.join("; "));
    }
    Ok(format!("{} {} container(s): done", op.verb(), ids.len()))
}

pub async fn remove_images(ep: &Endpoint, ids: &[String]) -> Result<String> {
    let mut failed = Vec::new();
    for id in ids {
        if let Err(e) = ep.json(Method::DELETE, &format!("/images/{}", enc(id)), None).await {
            failed.push(format!("{e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!("{} of {} image(s) not removed: {}", failed.len(), ids.len(), failed.join("; "));
    }
    Ok(format!("removed {} image(s)", ids.len()))
}

pub async fn remove_volumes(ep: &Endpoint, names: &[String]) -> Result<String> {
    let mut failed = Vec::new();
    for n in names {
        if let Err(e) = ep.json(Method::DELETE, &format!("/volumes/{}", enc(n)), None).await {
            failed.push(format!("{n}: {e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!("{} of {} volume(s) not removed: {}", failed.len(), names.len(), failed.join("; "));
    }
    Ok(format!("removed {} volume(s)", names.len()))
}

pub async fn prune_containers(ep: &Endpoint) -> Result<String> {
    let r = ep.json(Method::POST, "/containers/prune", None).await?;
    let n = r["ContainersDeleted"].as_array().map(Vec::len).unwrap_or(0);
    Ok(format!("removed {n} stopped container(s), {}", model::bytes(r["SpaceReclaimed"].as_u64().unwrap_or(0))))
}

pub async fn prune_build_cache(ep: &Endpoint) -> Result<String> {
    let r = ep.json(Method::POST, "/build/prune?all=1", None).await?;
    Ok(format!("build cache pruned, {}", model::bytes(r["SpaceReclaimed"].as_u64().unwrap_or(0))))
}

/// Follow a container's log (`docker logs -f --tail 1000`).
pub async fn logs(ep: Endpoint, id: String, log: u64, tx: UnboundedSender<Msg>) {
    let result: Result<()> = async {
        let inspect = ep.json(Method::GET, &format!("/containers/{id}/json"), None).await?;
        let tty = inspect.pointer("/Config/Tty").and_then(Value::as_bool).unwrap_or(false);
        let resp = ep.send(Method::GET, &format!("/containers/{id}/logs?follow=1&stdout=1&stderr=1&tail=1000"), None, Vec::new()).await?;
        if !resp.status().is_success() {
            bail!("logs: {}", resp.status());
        }
        pump_lines(Lines::new(resp.into_body()), !tty, log, &tx).await;
        Ok(())
    }
    .await;
    let _ = tx.send(Msg::LogEnd(log, result.err().map(|e| format!("{e:#}"))));
}

/// Read a log stream (Docker's multiplexed framing when `multiplexed`) and
/// send printable lines in batches.
pub async fn pump_lines(mut body: Lines, multiplexed: bool, id: u64, tx: &UnboundedSender<Msg>) {
    let mut demux = Demux::default();
    let mut text = Vec::new();
    loop {
        let Some(chunk) = body.chunk().await else { break };
        if multiplexed {
            demux.feed(&chunk, &mut text);
        } else {
            text.extend_from_slice(&chunk);
        }
        let mut lines = Vec::new();
        while let Some(pos) = text.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = text.drain(..=pos).collect();
            lines.push(printable(&line));
        }
        if !lines.is_empty() && tx.send(Msg::LogLines(id, lines)).is_err() {
            return;
        }
    }
    if !text.is_empty() {
        let _ = tx.send(Msg::LogLines(id, vec![printable(&text)]));
    }
}

/// Docker's stdout/stderr framing: 8-byte headers (stream, 0, 0, 0, size).
#[derive(Default)]
pub struct Demux {
    buf: Vec<u8>,
}

impl Demux {
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.buf.extend_from_slice(data);
        loop {
            if self.buf.len() < 8 {
                return;
            }
            let len = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            if self.buf[0] > 2 || self.buf[1..4] != [0, 0, 0] {
                // Not framed after all: pass it through.
                out.append(&mut self.buf);
                return;
            }
            if self.buf.len() < 8 + len {
                return;
            }
            out.extend_from_slice(&self.buf[8..8 + len]);
            self.buf.drain(..8 + len);
        }
    }
}

/// A log line as text the terminal can show: no escape sequences or control
/// characters (they would move the cursor over the UI), tabs expanded.
pub fn printable(line: &[u8]) -> String {
    // A log line longer than this is cut (the view keeps 5000 lines).
    const MAX: usize = 4096;
    let s = String::from_utf8_lossy(&line[..line.len().min(MAX * 4)]);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI (ESC [ ... final byte) or OSC (ESC ] ... BEL/ST); else
                // drop the next character.
                match chars.next() {
                    Some('[') => {
                        while let Some(n) = chars.next() {
                            if ('@'..='~').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        while let Some(n) = chars.next() {
                            if n == '\x07' || (n == '\x1b' && chars.peek() == Some(&'\\')) {
                                if n == '\x1b' {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            '\t' => out.push_str("    "),
            '\r' => {
                // A carriage return redraws the line: keep what comes after.
                if chars.peek().is_some_and(|n| *n != '\n') {
                    out.clear();
                }
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
        if out.len() >= MAX {
            out.push('…');
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demuxes_frames_split_anywhere() {
        let mut frames = Vec::new();
        for (stream, text) in [(1u8, &b"hello\n"[..]), (2, b"oops\n")] {
            frames.extend_from_slice(&[stream, 0, 0, 0]);
            frames.extend_from_slice(&(text.len() as u32).to_be_bytes());
            frames.extend_from_slice(text);
        }
        for split in 0..frames.len() {
            let mut d = Demux::default();
            let mut out = Vec::new();
            d.feed(&frames[..split], &mut out);
            d.feed(&frames[split..], &mut out);
            assert_eq!(out, b"hello\noops\n", "split at {split}");
        }
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(printable(b"\x1b[32mINFO\x1b[0m\tready\n"), "INFO    ready");
        assert_eq!(printable(b"\x1b]0;title\x07text"), "text");
        assert_eq!(printable(b"10%\r50%\r100%\n"), "100%");
        assert_eq!(printable(b"line\r\n"), "line");
        assert!(printable(&vec![b'x'; 100_000]).chars().count() <= 4097);
    }
}
