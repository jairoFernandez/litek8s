//! A shell in a pod's container over the API server's exec WebSocket
//! (`v5.channel.k8s.io`, else `v4.channel.k8s.io`: what kubectl speaks
//! since 1.30, and every API server has served v4 for years), while the UI
//! is suspended: keystrokes go to stdin (channel 0), stdout and stderr (1,
//! 2) to the terminal, window sizes to channel 4; channel 3 says how it
//! ended.

use std::io::Write;

use anyhow::{Context, Result, bail};
use base64::Engine;
use hyper::Method;
use rand_core::RngCore;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ui::client::{Endpoint, enc};

/// Prefer bash, as kubectl users' muscle memory does.
const SHELL: &str = "if command -v bash >/dev/null 2>&1; then exec bash; elif command -v ash >/dev/null 2>&1; then exec ash; else exec sh; fi";

/// A client WebSocket frame (masked, as clients must).
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n < 65536 {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    rand_core::OsRng.fill_bytes(&mut mask);
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// A message from the server: (opcode, payload), fragments joined; pings
/// answered by the caller.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut data = Vec::new();
    let mut first_op = None;
    loop {
        let mut h = [0u8; 2];
        r.read_exact(&mut h).await?;
        let fin = h[0] & 0x80 != 0;
        let op = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).await?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).await?;
            len = u64::from_be_bytes(b);
        }
        if len > 64 << 20 {
            bail!("WebSocket frame too large");
        }
        let mut mask = [0u8; 4];
        if masked {
            r.read_exact(&mut mask).await?;
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload).await?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        // Control frames come whole, between fragments.
        if op >= 0x8 {
            return Ok((op, payload));
        }
        if first_op.is_none() {
            first_op = Some(op);
        }
        data.extend_from_slice(&payload);
        if fin {
            return Ok((first_op.unwrap_or(op), data));
        }
    }
}

/// The exit code in a channel-3 Status (`Success`, or `NonZeroExitCode`
/// with an ExitCode cause).
pub fn exit_code(status: &[u8]) -> Result<i64> {
    let v: Value = serde_json::from_slice(status).context("not a Status")?;
    if v["status"].as_str() == Some("Success") {
        return Ok(0);
    }
    if let Some(code) = v.pointer("/details/causes").and_then(Value::as_array).and_then(|c| c.iter().find(|x| x["reason"].as_str() == Some("ExitCode"))).and_then(|x| x["message"].as_str()).and_then(|m| m.parse().ok()) {
        return Ok(code);
    }
    bail!("{}", v["message"].as_str().unwrap_or("the exec failed"))
}

pub async fn shell(ep: &Endpoint, ns: &str, pod: &str, container: Option<&str>) -> Result<i64> {
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let mut path = format!("/api/v1/namespaces/{ns}/pods/{pod}/exec?command=sh&command=-c&command={}&stdin=true&stdout=true&tty=true", enc(SHELL));
    if let Some(c) = container {
        path.push_str(&format!("&container={}", enc(c)));
    }
    let mut key = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut key);
    let key = base64::engine::general_purpose::STANDARD.encode(key);
    let headers = [("Sec-WebSocket-Key", key.as_str()), ("Sec-WebSocket-Version", "13"), ("Sec-WebSocket-Protocol", "v5.channel.k8s.io, v4.channel.k8s.io")];
    let (resp, io) = ep.upgrade(Method::GET, &path, "websocket", &headers, None).await?;
    let v5 = resp.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()) == Some("v5.channel.k8s.io");
    session(io, v5, cols, rows).await
}

async fn session<S: AsyncRead + AsyncWrite + Unpin + Send>(io: S, v5: bool, cols: u16, rows: u16) -> Result<i64> {
    let (mut from, to) = tokio::io::split(io);
    let to = std::sync::Arc::new(tokio::sync::Mutex::new(to));
    let send = |chan: u8, data: &[u8]| {
        let mut msg = vec![chan];
        msg.extend_from_slice(data);
        frame(0x2, &msg)
    };
    let resize = |c: u16, r: u16| send(4, format!("{{\"Width\":{c},\"Height\":{r}}}").as_bytes());
    to.lock().await.write_all(&resize(cols, rows)).await?;
    let mut keys = crate::ui::exec::Keys::start();
    let writer = to.clone();
    let input = async move {
        while let Some(k) = keys.rx.recv().await {
            let mut w = writer.lock().await;
            if w.write_all(&send(0, &k)).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
        keys
    };
    let resizer = to.clone();
    let winch = async move {
        let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) else { return std::future::pending::<()>().await };
        while sig.recv().await.is_some() {
            if let Ok((c, r)) = ratatui::crossterm::terminal::size() {
                let _ = resizer.lock().await.write_all(&resize(c, r)).await;
            }
        }
    };
    let ponger = to.clone();
    let output = async move {
        let mut out = std::io::stdout();
        let mut status: Option<Vec<u8>> = None;
        loop {
            let (op, data) = match read_message(&mut from).await {
                Ok(m) => m,
                Err(_) => break,
            };
            match op {
                0x8 => break,
                0x9 => {
                    let _ = ponger.lock().await.write_all(&frame(0xA, &data)).await;
                }
                0x1 | 0x2 if !data.is_empty() => match data[0] {
                    1 | 2 => {
                        let _ = out.write_all(&data[1..]);
                        let _ = out.flush();
                    }
                    3 => status = Some(data[1..].to_vec()),
                    _ => {}
                },
                _ => {}
            }
        }
        status
    };
    let _ = v5;
    tokio::pin!(input);
    let status = tokio::select! {
        s = output => s,
        _ = winch => None,
        keys = &mut input => { keys.stop().await; return Ok(0) }
    };
    // Stop reading keys before the UI takes the terminal back.
    drop(input);
    match status {
        Some(s) if !s.is_empty() => exit_code(&s),
        _ => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_masked() {
        let f = frame(0x2, b"\x00hello");
        assert_eq!(f[0], 0x82);
        assert_eq!(f[1] & 0x80, 0x80);
        let (op, data) = read_message(&mut &f[..]).await.unwrap();
        assert_eq!((op, data.as_slice()), (0x2, &b"\x00hello"[..]));
        let big = vec![7u8; 70_000];
        let (_, data) = read_message(&mut &frame(0x2, &big)[..]).await.unwrap();
        assert_eq!(data.len(), 70_000);
        // Fragments join; a ping between them comes on its own.
        let mut two = vec![0x02, 3, b'\x01', b'a', b'b', 0x80, 1, b'c'];
        two.extend_from_slice(&[0x89, 0]);
        let mut r = &two[..];
        let (op, data) = read_message(&mut r).await.unwrap();
        assert_eq!((op, data.as_slice()), (0x2, &b"\x01abc"[..]));
        assert_eq!(read_message(&mut r).await.unwrap().0, 0x9);
    }

    #[test]
    fn exit_codes_from_status() {
        assert_eq!(exit_code(br#"{"status":"Success"}"#).unwrap(), 0);
        assert_eq!(exit_code(br#"{"status":"Failure","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"3"}]}}"#).unwrap(), 3);
        assert!(exit_code(br#"{"status":"Failure","message":"no shell"}"#).unwrap_err().to_string().contains("no shell"));
    }
}
