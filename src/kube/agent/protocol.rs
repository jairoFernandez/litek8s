//! The wire protocol between the API server and node agents.
//!
//! One persistent, bidirectional byte stream per node (an upgraded HTTP
//! connection for remote nodes, an in-memory pipe for the local one) carries
//! length-prefixed frames:
//!
//! ```text
//! u32 length (big endian, of what follows) | u8 kind | u32 stream | payload
//! ```
//!
//! - kind 0, stream 0: a JSON [`Message`] (pod assignments, status, lookups)
//! - kind 1: stream data; the payload starts with a channel byte
//!   (0 stdin, 1 stdout, 2 stderr) followed by raw bytes
//! - kind 2: end of input on a stream (half close)
//!
//! Streams multiplex `kubectl logs`, `exec` and `port-forward` over the same
//! connection, so a remote node needs no inbound port and no extra
//! connections, and the API server never opens anything towards it.

use anyhow::{Context, Result, bail};
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted (pod objects and log chunks stay far below).
const MAX_FRAME: usize = 16 * 1024 * 1024;

pub const PROTOCOL_VERSION: &str = "litek8s-agent/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    // ---- agent → server
    /// First message: the Node object to register (labels, capacity, ...).
    Hello { node: Value },
    PodStatus { namespace: String, name: String, uid: String, status: Value },
    /// Look up an object a pod needs (configmaps, secrets, claims, volumes)
    /// or mint a service account token (`resource: "token"`).
    Get { id: u64, resource: String, namespace: String, name: String },
    /// A stream finished; for exec, with the command's exit code.
    StreamDone {
        stream: u32,
        #[serde(default)]
        exit_code: Option<i64>,
        #[serde(default)]
        error: Option<String>,
    },

    // ---- server → agent
    /// Accepted. `api_host`/`api_port` are where pods reach the API server.
    Welcome {
        node: String,
        api_host: String,
        api_port: u16,
        /// Cluster DNS answers on port 53 at `api_host`.
        #[serde(default)]
        cluster_dns: bool,
    },
    /// A pod bound to this node was created or changed.
    PodUpsert { pod: Value },
    PodDelete { uid: String },
    /// Every pod of this node has been sent; anything else running is stale.
    SyncDone,
    /// A PersistentVolume on this node was deleted: free its storage.
    VolumeDelete { name: String },
    /// A ConfigMap or Secret in a namespace with pods on this node changed:
    /// volumes that project it are refreshed.
    ObjectChanged { resource: String, namespace: String, name: String },
    GetResult {
        id: u64,
        #[serde(default)]
        object: Option<Value>,
    },
    Exec {
        stream: u32,
        pod_uid: String,
        container: String,
        command: Vec<String>,
        stdin: bool,
        tty: bool,
    },
    Logs {
        stream: u32,
        pod_uid: String,
        container: String,
        follow: bool,
        previous: bool,
        timestamps: bool,
        #[serde(default)]
        tail_lines: Option<i64>,
        #[serde(default)]
        since_seconds: Option<i64>,
        #[serde(default)]
        limit_bytes: Option<i64>,
    },
    PortForward { stream: u32, pod_uid: String, port: u16 },
    Resize { stream: u32, width: u16, height: u16 },
    /// The client went away: stop the stream.
    Cancel { stream: u32 },
}

#[derive(Debug, Clone)]
pub enum Frame {
    Control(Message),
    Data { stream: u32, channel: u8, bytes: Bytes },
    CloseInput { stream: u32 },
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let (kind, stream, payload): (u8, u32, Vec<u8>) = match frame {
        Frame::Control(msg) => (0, 0, serde_json::to_vec(msg)?),
        Frame::Data { stream, channel, bytes } => {
            let mut p = Vec::with_capacity(bytes.len() + 1);
            p.push(*channel);
            p.extend_from_slice(bytes);
            (1, *stream, p)
        }
        Frame::CloseInput { stream } => (2, *stream, Vec::new()),
    };
    let len = (payload.len() + 5) as u32;
    let mut header = [0u8; 9];
    header[..4].copy_from_slice(&len.to_be_bytes());
    header[4] = kind;
    header[5..9].copy_from_slice(&stream.to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(&payload).await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if !(5..=MAX_FRAME).contains(&len) {
        bail!("invalid frame length {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.context("truncated frame")?;
    let kind = buf[0];
    let stream = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let payload = &buf[5..];
    Ok(Some(match kind {
        0 => Frame::Control(serde_json::from_slice(payload).context("invalid control message")?),
        1 => {
            let (&channel, rest) = payload.split_first().context("empty data frame")?;
            Frame::Data { stream, channel, bytes: Bytes::copy_from_slice(rest) }
        }
        2 => Frame::CloseInput { stream },
        other => bail!("unknown frame kind {other}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let frames = vec![
            Frame::Control(Message::SyncDone),
            Frame::Data { stream: 7, channel: 1, bytes: Bytes::from_static(b"hello") },
            Frame::CloseInput { stream: 7 },
        ];
        for f in &frames {
            write_frame(&mut a, f).await.unwrap();
        }
        drop(a);
        assert!(matches!(read_frame(&mut b).await.unwrap(), Some(Frame::Control(Message::SyncDone))));
        match read_frame(&mut b).await.unwrap() {
            Some(Frame::Data { stream, channel, bytes }) => {
                assert_eq!((stream, channel, &bytes[..]), (7, 1, &b"hello"[..]));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(read_frame(&mut b).await.unwrap(), Some(Frame::CloseInput { stream: 7 })));
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
