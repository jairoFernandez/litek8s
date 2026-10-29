//! A minimal server-side WebSocket (RFC 6455), enough for the Kubernetes
//! streaming protocols kubectl speaks (`v5.channel.k8s.io` for exec).

use anyhow::{Result, bail};
use base64::Engine;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` value for a client key.
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.trim().as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

#[derive(Debug)]
pub enum Incoming {
    Binary(Vec<u8>),
    Text(Vec<u8>),
    Close,
}

pub struct Reader<R, W> {
    inner: R,
    /// The write half, shared with [`Writer`]: pings are answered from here.
    out: Arc<Mutex<W>>,
}

pub struct Writer<W> {
    inner: Arc<Mutex<W>>,
}

/// The two halves of an upgraded connection.
pub type Halves<S> = (Reader<tokio::io::ReadHalf<S>, tokio::io::WriteHalf<S>>, Writer<tokio::io::WriteHalf<S>>);

pub fn split<S: AsyncRead + AsyncWrite>(stream: S) -> Halves<S> {
    let (r, w) = tokio::io::split(stream);
    let out = Arc::new(Mutex::new(w));
    (Reader { inner: r, out: out.clone() }, Writer { inner: out })
}

/// One frame, written whole under the lock so pongs never interleave with data.
async fn write_frame<W: AsyncWrite + Unpin>(out: &Mutex<W>, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n <= u16::MAX as usize => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    let mut w = out.lock().await;
    w.write_all(&head).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Reader<R, W> {
    /// The next data message, reassembling fragments. Pings are answered with a
    /// pong carrying the same payload: client-go (kubectl 1.30+) sends one every
    /// few seconds and drops the stream when no pong comes back in time.
    pub async fn next(&mut self) -> Result<Incoming> {
        let mut message: Vec<u8> = Vec::new();
        let mut kind = 0u8;
        loop {
            let mut head = [0u8; 2];
            self.inner.read_exact(&mut head).await?;
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0f;
            let masked = head[1] & 0x80 != 0;
            let mut len = (head[1] & 0x7f) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.inner.read_exact(&mut b).await?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.inner.read_exact(&mut b).await?;
                len = u64::from_be_bytes(b);
            }
            if len > 64 * 1024 * 1024 {
                bail!("websocket frame too large");
            }
            let mut mask = [0u8; 4];
            if masked {
                self.inner.read_exact(&mut mask).await?;
            }
            let mut payload = vec![0u8; len as usize];
            self.inner.read_exact(&mut payload).await?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Ok(Incoming::Close),
                0x9 => {
                    write_frame(&self.out, 0xA, &payload).await?;
                    continue;
                }
                0xA => continue,
                0x0 => message.extend_from_slice(&payload),
                op => {
                    kind = op;
                    message = payload;
                }
            }
            if fin {
                return Ok(if kind == 0x1 { Incoming::Text(message) } else { Incoming::Binary(message) });
            }
        }
    }
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    async fn frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        write_frame(&self.inner, opcode, payload).await
    }

    pub async fn binary(&mut self, payload: &[u8]) -> Result<()> {
        self.frame(0x2, payload).await
    }

    pub async fn text(&mut self, payload: &[u8]) -> Result<()> {
        self.frame(0x1, payload).await
    }

    pub async fn close(&mut self) -> Result<()> {
        self.frame(0x8, &1000u16.to_be_bytes()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_example_accept_key() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn reads_masked_fragments() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _) = split(server);
        let (_, mut cw) = tokio::io::split(client);
        let mask = [1u8, 2, 3, 4];
        let masked = |data: &[u8]| -> Vec<u8> { data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect() };
        // "hel" (binary, not final) + "lo" (continuation, final)
        let mut bytes = vec![0x02, 0x80 | 3];
        bytes.extend_from_slice(&mask);
        bytes.extend(masked(b"hel"));
        bytes.extend([0x80, 0x80 | 2]);
        bytes.extend_from_slice(&mask);
        bytes.extend(masked(b"lo"));
        cw.write_all(&bytes).await.unwrap();
        match reader.next().await.unwrap() {
            Incoming::Binary(m) => assert_eq!(m, b"hello"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn answers_pings_with_pongs() {
        let (client, server) = tokio::io::duplex(1024);
        let (mut reader, _writer) = split(server);
        let (mut cr, mut cw) = tokio::io::split(client);
        let mask = [9u8, 8, 7, 6];
        let masked = |data: &[u8]| -> Vec<u8> { data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect() };
        // "a" (binary, not final), a ping "hb" (as client-go's heartbeat), "b" (final)
        let mut bytes = vec![0x02, 0x80 | 1];
        bytes.extend_from_slice(&mask);
        bytes.extend(masked(b"a"));
        bytes.extend([0x89, 0x80 | 2]);
        bytes.extend_from_slice(&mask);
        bytes.extend(masked(b"hb"));
        bytes.extend([0x80, 0x80 | 1]);
        bytes.extend_from_slice(&mask);
        bytes.extend(masked(b"b"));
        cw.write_all(&bytes).await.unwrap();
        match reader.next().await.unwrap() {
            Incoming::Binary(m) => assert_eq!(m, b"ab"),
            other => panic!("unexpected {other:?}"),
        }
        let mut pong = [0u8; 4];
        cr.read_exact(&mut pong).await.unwrap();
        assert_eq!(pong, [0x8A, 2, b'h', b'b']);
    }
}
