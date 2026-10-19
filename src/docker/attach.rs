//! Talking to a container's shim over its attach socket (see the shim's
//! `Attach`): frames `[kind u8][length u32 BE][payload]`; from the shim 1
//! stdout, 2 stderr, 3 exit code; to it 0 stdin, 1 close stdin, 2 resize.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn connect(sock: &Path, wait: Duration) -> std::io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match UnixStream::connect(sock).await {
            Ok(s) => return Ok(s),
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.push(kind);
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// Read one frame; `None` at the end of the stream.
pub async fn read_frame(r: &mut (impl AsyncReadExt + Unpin)) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).await.ok()?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    Some((head[0], buf))
}

pub async fn send_resize(sock: &Path, rows: u16, cols: u16) {
    if let Ok(mut s) = connect(sock, Duration::from_millis(500)).await {
        let _ = s.write_all(&frame(2, format!("{rows} {cols}").as_bytes())).await;
        let _ = s.shutdown().await;
    }
}

/// Docker's multiplexed stream header for `len` bytes of `stream`.
pub fn mux_header(stream: u8, len: usize) -> [u8; 8] {
    let l = (len as u32).to_be_bytes();
    [stream, 0, 0, 0, l[0], l[1], l[2], l[3]]
}

/// Watches client input for the detach sequence (default ctrl-p ctrl-q).
pub struct DetachKeys {
    keys: Vec<u8>,
    matched: usize,
}

impl DetachKeys {
    /// `ctrl-p,ctrl-q`, `ctrl-a,x`...; empty disables.
    pub fn parse(spec: &str) -> Self {
        let mut keys = Vec::new();
        for k in spec.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            if let Some(c) = k.strip_prefix("ctrl-") {
                let c = c.chars().next().unwrap_or('@');
                let code = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    '@' => 0,
                    '[' => 27,
                    '\\' => 28,
                    ']' => 29,
                    '^' => 30,
                    '_' => 31,
                    _ => continue,
                };
                keys.push(code);
            } else if k.len() == 1 {
                keys.push(k.as_bytes()[0]);
            }
        }
        Self { keys, matched: 0 }
    }

    /// Pass input through; `true` once the whole sequence was typed (the
    /// bytes that matched are held back).
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) -> bool {
        for &b in data {
            if self.keys.is_empty() {
                out.push(b);
                continue;
            }
            if b == self.keys[self.matched] {
                self.matched += 1;
                if self.matched == self.keys.len() {
                    return true;
                }
            } else {
                out.extend_from_slice(&self.keys[..self.matched]);
                self.matched = 0;
                if b == self.keys[0] {
                    self.matched = 1;
                } else {
                    out.push(b);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_sequence() {
        let mut d = DetachKeys::parse("ctrl-p,ctrl-q");
        let mut out = Vec::new();
        assert!(!d.feed(b"ab\x10c", &mut out));
        assert_eq!(out, b"ab\x10c");
        out.clear();
        assert!(d.feed(b"x\x10\x11", &mut out));
        assert_eq!(out, b"x");
        assert_eq!(mux_header(2, 258), [2, 0, 0, 0, 0, 0, 1, 2]);
    }
}
