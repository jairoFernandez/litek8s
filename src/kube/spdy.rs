//! Just enough SPDY/3.1 for `kubectl port-forward` (the
//! `portforward.k8s.io` protocol), either over a plain `SPDY/3.1` upgrade or
//! tunneled through a WebSocket (`SPDY/3.1+portforward.k8s.io`, what kubectl
//! 1.31+ tries first), and for exec/attach over a plain `SPDY/3.1` upgrade
//! (`v4.channel.k8s.io`, what client-go's `NewSPDYExecutor` speaks;
//! [`serve_exec`]).
//!
//! The client opens two streams per forwarded connection, an error stream
//! and a data stream, tied together by a `requestid` header. Data stream
//! bytes are relayed to the pod's port through its node agent; a failure to
//! connect is reported on the error stream. kubectl does no SPDY flow control
//! (spdystream ignores WINDOW_UPDATE), so neither does this.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use flate2::{Decompress, FlushDecompress, Status};
use hyper::body::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use super::agent::hub::{StreamEvent, StreamHandle};

/// The SPDY/3 header compression dictionary (from the SPDY/3 draft, as in
/// golang.org/x/net/spdy and moby/spdystream).
const DICTIONARY: &[u8] = &[
    0x00, 0x00, 0x00, 0x07, 0x6f, 0x70, 0x74, 0x69, 0x6f, 0x6e, 0x73, 0x00, 0x00, 0x00, 0x04, 0x68,
    0x65, 0x61, 0x64, 0x00, 0x00, 0x00, 0x04, 0x70, 0x6f, 0x73, 0x74, 0x00, 0x00, 0x00, 0x03, 0x70,
    0x75, 0x74, 0x00, 0x00, 0x00, 0x06, 0x64, 0x65, 0x6c, 0x65, 0x74, 0x65, 0x00, 0x00, 0x00, 0x05,
    0x74, 0x72, 0x61, 0x63, 0x65, 0x00, 0x00, 0x00, 0x06, 0x61, 0x63, 0x63, 0x65, 0x70, 0x74, 0x00,
    0x00, 0x00, 0x0e, 0x61, 0x63, 0x63, 0x65, 0x70, 0x74, 0x2d, 0x63, 0x68, 0x61, 0x72, 0x73, 0x65,
    0x74, 0x00, 0x00, 0x00, 0x0f, 0x61, 0x63, 0x63, 0x65, 0x70, 0x74, 0x2d, 0x65, 0x6e, 0x63, 0x6f,
    0x64, 0x69, 0x6e, 0x67, 0x00, 0x00, 0x00, 0x0f, 0x61, 0x63, 0x63, 0x65, 0x70, 0x74, 0x2d, 0x6c,
    0x61, 0x6e, 0x67, 0x75, 0x61, 0x67, 0x65, 0x00, 0x00, 0x00, 0x0d, 0x61, 0x63, 0x63, 0x65, 0x70,
    0x74, 0x2d, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x73, 0x00, 0x00, 0x00, 0x03, 0x61, 0x67, 0x65, 0x00,
    0x00, 0x00, 0x05, 0x61, 0x6c, 0x6c, 0x6f, 0x77, 0x00, 0x00, 0x00, 0x0d, 0x61, 0x75, 0x74, 0x68,
    0x6f, 0x72, 0x69, 0x7a, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00, 0x0d, 0x63, 0x61, 0x63,
    0x68, 0x65, 0x2d, 0x63, 0x6f, 0x6e, 0x74, 0x72, 0x6f, 0x6c, 0x00, 0x00, 0x00, 0x0a, 0x63, 0x6f,
    0x6e, 0x6e, 0x65, 0x63, 0x74, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00, 0x0c, 0x63, 0x6f, 0x6e, 0x74,
    0x65, 0x6e, 0x74, 0x2d, 0x62, 0x61, 0x73, 0x65, 0x00, 0x00, 0x00, 0x10, 0x63, 0x6f, 0x6e, 0x74,
    0x65, 0x6e, 0x74, 0x2d, 0x65, 0x6e, 0x63, 0x6f, 0x64, 0x69, 0x6e, 0x67, 0x00, 0x00, 0x00, 0x10,
    0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x6c, 0x61, 0x6e, 0x67, 0x75, 0x61, 0x67, 0x65,
    0x00, 0x00, 0x00, 0x0e, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x6c, 0x65, 0x6e, 0x67,
    0x74, 0x68, 0x00, 0x00, 0x00, 0x10, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x6c, 0x6f,
    0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00, 0x0b, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e,
    0x74, 0x2d, 0x6d, 0x64, 0x35, 0x00, 0x00, 0x00, 0x0d, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74,
    0x2d, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x00, 0x00, 0x00, 0x0c, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e,
    0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x00, 0x00, 0x00, 0x04, 0x64, 0x61, 0x74, 0x65, 0x00, 0x00,
    0x00, 0x04, 0x65, 0x74, 0x61, 0x67, 0x00, 0x00, 0x00, 0x06, 0x65, 0x78, 0x70, 0x65, 0x63, 0x74,
    0x00, 0x00, 0x00, 0x07, 0x65, 0x78, 0x70, 0x69, 0x72, 0x65, 0x73, 0x00, 0x00, 0x00, 0x04, 0x66,
    0x72, 0x6f, 0x6d, 0x00, 0x00, 0x00, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x00, 0x00, 0x00, 0x08, 0x69,
    0x66, 0x2d, 0x6d, 0x61, 0x74, 0x63, 0x68, 0x00, 0x00, 0x00, 0x11, 0x69, 0x66, 0x2d, 0x6d, 0x6f,
    0x64, 0x69, 0x66, 0x69, 0x65, 0x64, 0x2d, 0x73, 0x69, 0x6e, 0x63, 0x65, 0x00, 0x00, 0x00, 0x0d,
    0x69, 0x66, 0x2d, 0x6e, 0x6f, 0x6e, 0x65, 0x2d, 0x6d, 0x61, 0x74, 0x63, 0x68, 0x00, 0x00, 0x00,
    0x08, 0x69, 0x66, 0x2d, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x00, 0x00, 0x00, 0x13, 0x69, 0x66, 0x2d,
    0x75, 0x6e, 0x6d, 0x6f, 0x64, 0x69, 0x66, 0x69, 0x65, 0x64, 0x2d, 0x73, 0x69, 0x6e, 0x63, 0x65,
    0x00, 0x00, 0x00, 0x0d, 0x6c, 0x61, 0x73, 0x74, 0x2d, 0x6d, 0x6f, 0x64, 0x69, 0x66, 0x69, 0x65,
    0x64, 0x00, 0x00, 0x00, 0x08, 0x6c, 0x6f, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00,
    0x0c, 0x6d, 0x61, 0x78, 0x2d, 0x66, 0x6f, 0x72, 0x77, 0x61, 0x72, 0x64, 0x73, 0x00, 0x00, 0x00,
    0x06, 0x70, 0x72, 0x61, 0x67, 0x6d, 0x61, 0x00, 0x00, 0x00, 0x12, 0x70, 0x72, 0x6f, 0x78, 0x79,
    0x2d, 0x61, 0x75, 0x74, 0x68, 0x65, 0x6e, 0x74, 0x69, 0x63, 0x61, 0x74, 0x65, 0x00, 0x00, 0x00,
    0x13, 0x70, 0x72, 0x6f, 0x78, 0x79, 0x2d, 0x61, 0x75, 0x74, 0x68, 0x6f, 0x72, 0x69, 0x7a, 0x61,
    0x74, 0x69, 0x6f, 0x6e, 0x00, 0x00, 0x00, 0x05, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x00, 0x00, 0x00,
    0x07, 0x72, 0x65, 0x66, 0x65, 0x72, 0x65, 0x72, 0x00, 0x00, 0x00, 0x0b, 0x72, 0x65, 0x74, 0x72,
    0x79, 0x2d, 0x61, 0x66, 0x74, 0x65, 0x72, 0x00, 0x00, 0x00, 0x06, 0x73, 0x65, 0x72, 0x76, 0x65,
    0x72, 0x00, 0x00, 0x00, 0x02, 0x74, 0x65, 0x00, 0x00, 0x00, 0x07, 0x74, 0x72, 0x61, 0x69, 0x6c,
    0x65, 0x72, 0x00, 0x00, 0x00, 0x11, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x66, 0x65, 0x72, 0x2d, 0x65,
    0x6e, 0x63, 0x6f, 0x64, 0x69, 0x6e, 0x67, 0x00, 0x00, 0x00, 0x07, 0x75, 0x70, 0x67, 0x72, 0x61,
    0x64, 0x65, 0x00, 0x00, 0x00, 0x0a, 0x75, 0x73, 0x65, 0x72, 0x2d, 0x61, 0x67, 0x65, 0x6e, 0x74,
    0x00, 0x00, 0x00, 0x04, 0x76, 0x61, 0x72, 0x79, 0x00, 0x00, 0x00, 0x03, 0x76, 0x69, 0x61, 0x00,
    0x00, 0x00, 0x07, 0x77, 0x61, 0x72, 0x6e, 0x69, 0x6e, 0x67, 0x00, 0x00, 0x00, 0x10, 0x77, 0x77,
    0x77, 0x2d, 0x61, 0x75, 0x74, 0x68, 0x65, 0x6e, 0x74, 0x69, 0x63, 0x61, 0x74, 0x65, 0x00, 0x00,
    0x00, 0x06, 0x6d, 0x65, 0x74, 0x68, 0x6f, 0x64, 0x00, 0x00, 0x00, 0x03, 0x67, 0x65, 0x74, 0x00,
    0x00, 0x00, 0x06, 0x73, 0x74, 0x61, 0x74, 0x75, 0x73, 0x00, 0x00, 0x00, 0x06, 0x32, 0x30, 0x30,
    0x20, 0x4f, 0x4b, 0x00, 0x00, 0x00, 0x07, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6f, 0x6e, 0x00, 0x00,
    0x00, 0x08, 0x48, 0x54, 0x54, 0x50, 0x2f, 0x31, 0x2e, 0x31, 0x00, 0x00, 0x00, 0x03, 0x75, 0x72,
    0x6c, 0x00, 0x00, 0x00, 0x06, 0x70, 0x75, 0x62, 0x6c, 0x69, 0x63, 0x00, 0x00, 0x00, 0x0a, 0x73,
    0x65, 0x74, 0x2d, 0x63, 0x6f, 0x6f, 0x6b, 0x69, 0x65, 0x00, 0x00, 0x00, 0x0a, 0x6b, 0x65, 0x65,
    0x70, 0x2d, 0x61, 0x6c, 0x69, 0x76, 0x65, 0x00, 0x00, 0x00, 0x06, 0x6f, 0x72, 0x69, 0x67, 0x69,
    0x6e, 0x31, 0x30, 0x30, 0x31, 0x30, 0x31, 0x32, 0x30, 0x31, 0x32, 0x30, 0x32, 0x32, 0x30, 0x35,
    0x32, 0x30, 0x36, 0x33, 0x30, 0x30, 0x33, 0x30, 0x32, 0x33, 0x30, 0x33, 0x33, 0x30, 0x34, 0x33,
    0x30, 0x35, 0x33, 0x30, 0x36, 0x33, 0x30, 0x37, 0x34, 0x30, 0x32, 0x34, 0x30, 0x35, 0x34, 0x30,
    0x36, 0x34, 0x30, 0x37, 0x34, 0x30, 0x38, 0x34, 0x30, 0x39, 0x34, 0x31, 0x30, 0x34, 0x31, 0x31,
    0x34, 0x31, 0x32, 0x34, 0x31, 0x33, 0x34, 0x31, 0x34, 0x34, 0x31, 0x35, 0x34, 0x31, 0x36, 0x34,
    0x31, 0x37, 0x35, 0x30, 0x32, 0x35, 0x30, 0x34, 0x35, 0x30, 0x35, 0x32, 0x30, 0x33, 0x20, 0x4e,
    0x6f, 0x6e, 0x2d, 0x41, 0x75, 0x74, 0x68, 0x6f, 0x72, 0x69, 0x74, 0x61, 0x74, 0x69, 0x76, 0x65,
    0x20, 0x49, 0x6e, 0x66, 0x6f, 0x72, 0x6d, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x32, 0x30, 0x34, 0x20,
    0x4e, 0x6f, 0x20, 0x43, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x33, 0x30, 0x31, 0x20, 0x4d, 0x6f,
    0x76, 0x65, 0x64, 0x20, 0x50, 0x65, 0x72, 0x6d, 0x61, 0x6e, 0x65, 0x6e, 0x74, 0x6c, 0x79, 0x34,
    0x30, 0x30, 0x20, 0x42, 0x61, 0x64, 0x20, 0x52, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74, 0x34, 0x30,
    0x31, 0x20, 0x55, 0x6e, 0x61, 0x75, 0x74, 0x68, 0x6f, 0x72, 0x69, 0x7a, 0x65, 0x64, 0x34, 0x30,
    0x33, 0x20, 0x46, 0x6f, 0x72, 0x62, 0x69, 0x64, 0x64, 0x65, 0x6e, 0x34, 0x30, 0x34, 0x20, 0x4e,
    0x6f, 0x74, 0x20, 0x46, 0x6f, 0x75, 0x6e, 0x64, 0x35, 0x30, 0x30, 0x20, 0x49, 0x6e, 0x74, 0x65,
    0x72, 0x6e, 0x61, 0x6c, 0x20, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72, 0x20, 0x45, 0x72, 0x72, 0x6f,
    0x72, 0x35, 0x30, 0x31, 0x20, 0x4e, 0x6f, 0x74, 0x20, 0x49, 0x6d, 0x70, 0x6c, 0x65, 0x6d, 0x65,
    0x6e, 0x74, 0x65, 0x64, 0x35, 0x30, 0x33, 0x20, 0x53, 0x65, 0x72, 0x76, 0x69, 0x63, 0x65, 0x20,
    0x55, 0x6e, 0x61, 0x76, 0x61, 0x69, 0x6c, 0x61, 0x62, 0x6c, 0x65, 0x4a, 0x61, 0x6e, 0x20, 0x46,
    0x65, 0x62, 0x20, 0x4d, 0x61, 0x72, 0x20, 0x41, 0x70, 0x72, 0x20, 0x4d, 0x61, 0x79, 0x20, 0x4a,
    0x75, 0x6e, 0x20, 0x4a, 0x75, 0x6c, 0x20, 0x41, 0x75, 0x67, 0x20, 0x53, 0x65, 0x70, 0x74, 0x20,
    0x4f, 0x63, 0x74, 0x20, 0x4e, 0x6f, 0x76, 0x20, 0x44, 0x65, 0x63, 0x20, 0x30, 0x30, 0x3a, 0x30,
    0x30, 0x3a, 0x30, 0x30, 0x20, 0x4d, 0x6f, 0x6e, 0x2c, 0x20, 0x54, 0x75, 0x65, 0x2c, 0x20, 0x57,
    0x65, 0x64, 0x2c, 0x20, 0x54, 0x68, 0x75, 0x2c, 0x20, 0x46, 0x72, 0x69, 0x2c, 0x20, 0x53, 0x61,
    0x74, 0x2c, 0x20, 0x53, 0x75, 0x6e, 0x2c, 0x20, 0x47, 0x4d, 0x54, 0x63, 0x68, 0x75, 0x6e, 0x6b,
    0x65, 0x64, 0x2c, 0x74, 0x65, 0x78, 0x74, 0x2f, 0x68, 0x74, 0x6d, 0x6c, 0x2c, 0x69, 0x6d, 0x61,
    0x67, 0x65, 0x2f, 0x70, 0x6e, 0x67, 0x2c, 0x69, 0x6d, 0x61, 0x67, 0x65, 0x2f, 0x6a, 0x70, 0x67,
    0x2c, 0x69, 0x6d, 0x61, 0x67, 0x65, 0x2f, 0x67, 0x69, 0x66, 0x2c, 0x61, 0x70, 0x70, 0x6c, 0x69,
    0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x78, 0x6d, 0x6c, 0x2c, 0x61, 0x70, 0x70, 0x6c, 0x69,
    0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x78, 0x68, 0x74, 0x6d, 0x6c, 0x2b, 0x78, 0x6d, 0x6c,
    0x2c, 0x74, 0x65, 0x78, 0x74, 0x2f, 0x70, 0x6c, 0x61, 0x69, 0x6e, 0x2c, 0x74, 0x65, 0x78, 0x74,
    0x2f, 0x6a, 0x61, 0x76, 0x61, 0x73, 0x63, 0x72, 0x69, 0x70, 0x74, 0x2c, 0x70, 0x75, 0x62, 0x6c,
    0x69, 0x63, 0x70, 0x72, 0x69, 0x76, 0x61, 0x74, 0x65, 0x6d, 0x61, 0x78, 0x2d, 0x61, 0x67, 0x65,
    0x3d, 0x67, 0x7a, 0x69, 0x70, 0x2c, 0x64, 0x65, 0x66, 0x6c, 0x61, 0x74, 0x65, 0x2c, 0x73, 0x64,
    0x63, 0x68, 0x63, 0x68, 0x61, 0x72, 0x73, 0x65, 0x74, 0x3d, 0x75, 0x74, 0x66, 0x2d, 0x38, 0x63,
    0x68, 0x61, 0x72, 0x73, 0x65, 0x74, 0x3d, 0x69, 0x73, 0x6f, 0x2d, 0x38, 0x38, 0x35, 0x39, 0x2d,
    0x31, 0x2c, 0x75, 0x74, 0x66, 0x2d, 0x2c, 0x2a, 0x2c, 0x65, 0x6e, 0x71, 0x3d, 0x30, 0x2e,];

const TYPE_SYN_STREAM: u16 = 1;
const TYPE_SYN_REPLY: u16 = 2;
const TYPE_RST_STREAM: u16 = 3;
const TYPE_PING: u16 = 6;
const TYPE_GOAWAY: u16 = 7;
const FLAG_FIN: u8 = 0x01;

/// Opens a relay to a port of the pod.
pub type Opener = Arc<dyn Fn(u16) -> futures_util::future::BoxFuture<'static, Result<StreamHandle>> + Send + Sync>;

fn control(frame_type: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(0x8000u16 | 3).to_be_bytes());
    out.extend_from_slice(&frame_type.to_be_bytes());
    let len = payload.len() as u32;
    out.push(flags);
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.extend_from_slice(payload);
    out
}

fn data(stream: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(payload);
    out
}

/// Header blocks we send: a zlib stream of stored (uncompressed) deflate
/// blocks, which every inflater accepts without needing the dictionary.
struct HeaderWriter {
    started: bool,
}

impl HeaderWriter {
    fn block(&mut self, headers: &[(&str, &str)]) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        for (k, v) in headers {
            raw.extend_from_slice(&(k.len() as u32).to_be_bytes());
            raw.extend_from_slice(k.as_bytes());
            raw.extend_from_slice(&(v.len() as u32).to_be_bytes());
            raw.extend_from_slice(v.as_bytes());
        }
        let mut out = Vec::with_capacity(raw.len() + 8);
        if !self.started {
            out.extend_from_slice(&[0x78, 0x01]);
            self.started = true;
        }
        // A non-final stored block: BFINAL=0, BTYPE=00, LEN, NLEN, data.
        out.push(0x00);
        let len = raw.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&raw);
        // Then an empty stored block, as a zlib sync flush ends: Go's inflater
        // only hands out the data once it has seen the next block start.
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0xff, 0xff]);
        out
    }
}

/// Header blocks the client sends: one zlib stream (with the SPDY
/// dictionary) across all frames of the connection.
struct HeaderReader {
    inflate: Decompress,
}

impl HeaderReader {
    fn new() -> Self {
        Self { inflate: Decompress::new(true) }
    }

    fn block(&mut self, mut input: &[u8]) -> Result<HashMap<String, String>> {
        let mut raw = Vec::with_capacity(input.len() * 4 + 64);
        while !input.is_empty() {
            let before_in = self.inflate.total_in();
            raw.reserve(4096);
            let result = self.inflate.decompress_vec(input, &mut raw, FlushDecompress::Sync);
            let consumed = (self.inflate.total_in() - before_in) as usize;
            input = &input[consumed..];
            match result {
                Ok(Status::Ok | Status::BufError) if consumed == 0 && raw.capacity() > raw.len() => break,
                Ok(_) => {}
                Err(e) if e.needs_dictionary().is_some() => {
                    self.inflate.set_dictionary(DICTIONARY)?;
                }
                Err(e) => bail!("bad SPDY header block: {e}"),
            }
        }
        let mut headers = HashMap::new();
        let mut r = &raw[..];
        let mut take = |n: usize| -> Result<&[u8]> {
            if r.len() < n {
                bail!("truncated SPDY header block");
            }
            let (a, b) = r.split_at(n);
            r = b;
            Ok(a)
        };
        let count = u32::from_be_bytes(take(4)?.try_into()?);
        for _ in 0..count {
            let n = u32::from_be_bytes(take(4)?.try_into()?) as usize;
            let name = String::from_utf8_lossy(take(n)?).to_lowercase();
            let n = u32::from_be_bytes(take(4)?.try_into()?) as usize;
            let value = String::from_utf8_lossy(take(n)?).to_string();
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

enum Kind {
    Error,
    Data(mpsc::UnboundedSender<Option<Bytes>>),
}

/// Serve port-forward streams on `io` until the client goes away.
pub async fn serve<S>(io: S, open: Opener) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(io);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            if out_rx.is_empty() && writer.flush().await.is_err() {
                break;
            }
        }
    });
    let mut headers_out = HeaderWriter { started: false };
    let mut headers_in = HeaderReader::new();
    let mut streams: HashMap<u32, Kind> = HashMap::new();
    // requestid → error stream id, so a data stream can report failures.
    let mut error_streams: HashMap<String, u32> = HashMap::new();
    let mut tasks = Vec::new();

    let result = async {
        loop {
            let mut head = [0u8; 8];
            match reader.read_exact(&mut head).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let flags = head[4];
            let len = u32::from_be_bytes([0, head[5], head[6], head[7]]) as usize;
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload).await?;
            if head[0] & 0x80 == 0 {
                let sid = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) & 0x7fff_ffff;
                if let Some(Kind::Data(tx)) = streams.get(&sid) {
                    if !payload.is_empty() {
                        let _ = tx.send(Some(Bytes::from(payload)));
                    }
                    if flags & FLAG_FIN != 0 {
                        let _ = tx.send(None);
                    }
                }
                continue;
            }
            let frame_type = u16::from_be_bytes([head[2], head[3]]);
            match frame_type {
                TYPE_SYN_STREAM => {
                    if payload.len() < 10 {
                        bail!("short SYN_STREAM");
                    }
                    let sid = u32::from_be_bytes(payload[0..4].try_into()?) & 0x7fff_ffff;
                    let headers = headers_in.block(&payload[10..])?;
                    let reply = headers_out.block(&[]);
                    let mut body = sid.to_be_bytes().to_vec();
                    body.extend_from_slice(&reply);
                    let _ = out_tx.send(control(TYPE_SYN_REPLY, 0, &body));
                    let request = headers.get("requestid").cloned().unwrap_or_default();
                    match headers.get("streamtype").map(String::as_str) {
                        Some("error") => {
                            error_streams.insert(request, sid);
                            streams.insert(sid, Kind::Error);
                        }
                        Some("data") => {
                            let port: u16 = headers.get("port").and_then(|p| p.parse().ok()).unwrap_or(0);
                            let (tx, rx) = mpsc::unbounded_channel();
                            streams.insert(sid, Kind::Data(tx));
                            let error_sid = error_streams.get(&request).copied();
                            tasks.push(tokio::spawn(relay(open.clone(), port, sid, error_sid, rx, out_tx.clone())));
                        }
                        _ => {
                            let mut rst = sid.to_be_bytes().to_vec();
                            rst.extend_from_slice(&1u32.to_be_bytes());
                            let _ = out_tx.send(control(TYPE_RST_STREAM, 0, &rst));
                        }
                    }
                }
                TYPE_PING => {
                    let _ = out_tx.send(control(TYPE_PING, 0, &payload));
                }
                TYPE_RST_STREAM => {
                    if payload.len() >= 4 {
                        let sid = u32::from_be_bytes(payload[0..4].try_into()?) & 0x7fff_ffff;
                        if let Some(Kind::Data(tx)) = streams.remove(&sid) {
                            let _ = tx.send(None);
                        }
                    }
                }
                TYPE_GOAWAY => return Ok(()),
                // SETTINGS, WINDOW_UPDATE, HEADERS, SYN_REPLY: nothing to do.
                _ => {}
            }
        }
    }
    .await;
    for t in tasks {
        t.abort();
    }
    writer_task.abort();
    result
}

/// Which streams an exec client opens (from the request's query).
#[derive(Debug, Clone, Copy)]
pub struct ExecStreams {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub tty: bool,
}

impl ExecStreams {
    fn expected(&self) -> Vec<&'static str> {
        let mut out = vec!["error"];
        if self.stdin {
            out.push("stdin");
        }
        if self.stdout {
            out.push("stdout");
        }
        if self.stderr && !self.tty {
            out.push("stderr");
        }
        if self.tty {
            out.push("resize");
        }
        out
    }
}

/// exec/attach over SPDY: the client opens one stream per channel
/// (`streamtype` error, stdin, stdout, stderr, resize); once they are all
/// there, the node's output goes to stdout/stderr, stdin and resizes go to
/// the node, and the end is a Status on the error stream (JSON with
/// `v4.channel.k8s.io`, the error text before it: `json_status` false).
pub async fn serve_exec<S>(io: S, mut handle: StreamHandle, want: ExecStreams, json_status: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(io);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            if out_rx.is_empty() && writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    let expected = want.expected();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<HashMap<String, u32>>();
    let input = handle.input();
    let replies = out_tx.clone();
    let reader_task = tokio::spawn(async move {
        let mut headers_out = HeaderWriter { started: false };
        let mut headers_in = HeaderReader::new();
        let mut by_type: HashMap<String, u32> = HashMap::new();
        let mut ready_tx = Some(ready_tx);
        let result: Result<()> = async {
            loop {
                let mut head = [0u8; 8];
                match reader.read_exact(&mut head).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
                let flags = head[4];
                let len = u32::from_be_bytes([0, head[5], head[6], head[7]]) as usize;
                let mut payload = vec![0u8; len];
                reader.read_exact(&mut payload).await?;
                if head[0] & 0x80 == 0 {
                    let sid = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) & 0x7fff_ffff;
                    if by_type.get("stdin") == Some(&sid) {
                        if !payload.is_empty() {
                            input.send(0, Bytes::from(payload)).await;
                        }
                        if flags & FLAG_FIN != 0 {
                            input.close().await;
                        }
                    } else if by_type.get("resize") == Some(&sid) {
                        // A JSON object per resize, possibly several per frame.
                        let sizes: Vec<(u16, u16)> = serde_json::Deserializer::from_slice(&payload)
                            .into_iter::<serde_json::Value>()
                            .flatten()
                            .map(|size| {
                                let w = size.get("Width").and_then(serde_json::Value::as_u64).unwrap_or(80) as u16;
                                let h = size.get("Height").and_then(serde_json::Value::as_u64).unwrap_or(24) as u16;
                                (w, h)
                            })
                            .collect();
                        for (w, h) in sizes {
                            input.resize(w, h).await;
                        }
                    }
                    continue;
                }
                match u16::from_be_bytes([head[2], head[3]]) {
                    TYPE_SYN_STREAM => {
                        if payload.len() < 10 {
                            bail!("short SYN_STREAM");
                        }
                        let sid = u32::from_be_bytes(payload[0..4].try_into()?) & 0x7fff_ffff;
                        let headers = headers_in.block(&payload[10..])?;
                        let mut body = sid.to_be_bytes().to_vec();
                        body.extend_from_slice(&headers_out.block(&[]));
                        let _ = replies.send(control(TYPE_SYN_REPLY, 0, &body));
                        let kind = headers.get("streamtype").cloned().unwrap_or_default();
                        by_type.insert(kind, sid);
                        if expected.iter().all(|t| by_type.contains_key(*t)) {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(by_type.clone());
                            }
                        }
                    }
                    TYPE_PING => {
                        let _ = replies.send(control(TYPE_PING, 0, &payload));
                    }
                    TYPE_GOAWAY => return Ok(()),
                    _ => {}
                }
            }
        }
        .await;
        input.close().await;
        result
    });
    // The kubelet waits as long for the client's streams.
    let ids = match tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx).await {
        Ok(Ok(ids)) => ids,
        _ => {
            reader_task.abort();
            writer_task.abort();
            bail!("exec client did not open its streams");
        }
    };
    let stdout = ids.get("stdout").copied();
    let stderr = ids.get("stderr").copied().or(if want.tty { stdout } else { None });
    let error = ids["error"];
    let mut status = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            StreamEvent::Data { channel, bytes } => {
                let target = if channel == 2 { stderr } else { stdout };
                if let Some(sid) = target {
                    for chunk in bytes.chunks(32 * 1024) {
                        let _ = out_tx.send(data(sid, 0, chunk));
                    }
                }
            }
            StreamEvent::Done { exit_code, error } => {
                status = Some((exit_code, error));
                break;
            }
        }
    }
    let (exit_code, err) = status.unwrap_or((None, Some("the exec stream ended".into())));
    let message = match (&exit_code, &err) {
        (_, Some(e)) => Some(serde_json::json!({"metadata": {}, "status": "Failure", "message": e, "reason": "InternalError", "code": 500})),
        (Some(0) | None, None) => None,
        (Some(code), None) => Some(serde_json::json!({
            "metadata": {},
            "status": "Failure",
            "message": format!("command terminated with non-zero exit code: exit status {code}"),
            "reason": "NonZeroExitCode",
            "details": {"causes": [{"reason": "ExitCode", "message": code.to_string()}]},
        })),
    };
    let body = match (message, json_status) {
        (Some(s), true) => s.to_string().into_bytes(),
        (None, true) => serde_json::json!({"metadata": {}, "status": "Success"}).to_string().into_bytes(),
        (Some(s), false) => s["message"].as_str().unwrap_or_default().as_bytes().to_vec(),
        (None, false) => Vec::new(),
    };
    for sid in [stdout, ids.get("stderr").copied()].into_iter().flatten() {
        let _ = out_tx.send(data(sid, FLAG_FIN, &[]));
    }
    if !body.is_empty() {
        let _ = out_tx.send(data(error, 0, &body));
    }
    let _ = out_tx.send(data(error, FLAG_FIN, &[]));
    drop(out_tx);
    // The client hangs up once it has the status; give it the time to.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), reader_task).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), writer_task).await;
    Ok(())
}

/// The client side, for one forwarded connection: `conn`'s bytes go to the
/// pod's `port` over the SPDY session on `io` (an upgraded
/// `POST .../pods/<name>/portforward`) and back. Errors the server reports
/// on the error stream are returned.
pub async fn forward_one<S, C>(io: S, port: u16, conn: C) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    C: AsyncRead + AsyncWrite + Send + 'static,
{
    const ERROR_SID: u32 = 1;
    const DATA_SID: u32 = 3;
    let (mut reader, mut writer) = tokio::io::split(io);
    let (mut local_r, mut local_w) = tokio::io::split(conn);
    let mut headers = HeaderWriter { started: false };
    let port_s = port.to_string();
    for (sid, kind) in [(ERROR_SID, "error"), (DATA_SID, "data")] {
        let mut body = sid.to_be_bytes().to_vec();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&headers.block(&[("streamtype", kind), ("port", &port_s), ("requestid", "0")]));
        writer.write_all(&control(TYPE_SYN_STREAM, 0, &body)).await?;
    }
    writer.flush().await?;
    let upload = async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = local_r.read(&mut buf).await?;
            writer.write_all(&data(DATA_SID, if n == 0 { FLAG_FIN } else { 0 }, &buf[..n])).await?;
            writer.flush().await?;
            if n == 0 {
                // Keep the session (and the download) open until the pod side ends.
                return std::future::pending::<Result<()>>().await;
            }
        }
    };
    let download = async move {
        let mut error = Vec::new();
        loop {
            let mut head = [0u8; 8];
            if reader.read_exact(&mut head).await.is_err() {
                break;
            }
            let flags = head[4];
            let len = u32::from_be_bytes([0, head[5], head[6], head[7]]) as usize;
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload).await?;
            if head[0] & 0x80 != 0 {
                let frame_type = u16::from_be_bytes([head[2], head[3]]);
                if frame_type == TYPE_RST_STREAM || frame_type == TYPE_GOAWAY {
                    break;
                }
                continue;
            }
            let sid = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) & 0x7fff_ffff;
            if sid == ERROR_SID {
                error.extend_from_slice(&payload);
            } else if sid == DATA_SID {
                local_w.write_all(&payload).await?;
                if flags & FLAG_FIN != 0 {
                    break;
                }
            }
        }
        let _ = local_w.shutdown().await;
        if !error.is_empty() {
            bail!("{}", String::from_utf8_lossy(&error).trim());
        }
        Ok(())
    };
    tokio::select! {
        r = upload => r,
        r = download => r,
    }
}

/// One forwarded connection: client bytes → pod, pod bytes → client.
async fn relay(
    open: Opener,
    port: u16,
    sid: u32,
    error_sid: Option<u32>,
    mut from_client: mpsc::UnboundedReceiver<Option<Bytes>>,
    out: mpsc::UnboundedSender<Vec<u8>>,
) {
    let fail = |message: String| {
        if let Some(e) = error_sid {
            let _ = out.send(data(e, 0, message.as_bytes()));
            let _ = out.send(data(e, FLAG_FIN, &[]));
        }
        let _ = out.send(data(sid, FLAG_FIN, &[]));
    };
    let mut handle = match open(port).await {
        Ok(h) => h,
        Err(e) => return fail(format!("{e:#}")),
    };
    let input = handle.input();
    let pump = tokio::spawn(async move {
        while let Some(chunk) = from_client.recv().await {
            match chunk {
                Some(bytes) => {
                    if !input.send(0, bytes).await {
                        break;
                    }
                }
                None => {
                    input.close().await;
                    break;
                }
            }
        }
    });
    while let Some(event) = handle.events.recv().await {
        match event {
            // Channel 2 is the agent's stderr when it relays through the pod sandbox.
            StreamEvent::Data { channel: 2, .. } => {}
            StreamEvent::Data { bytes, .. } => {
                for chunk in bytes.chunks(32 * 1024) {
                    let _ = out.send(data(sid, 0, chunk));
                }
            }
            StreamEvent::Done { error: Some(e), .. } => {
                pump.abort();
                return fail(e);
            }
            StreamEvent::Done { .. } => break,
        }
    }
    pump.abort();
    if let Some(e) = error_sid {
        let _ = out.send(data(e, FLAG_FIN, &[]));
    }
    let _ = out.send(data(sid, FLAG_FIN, &[]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_is_the_spdy3_one() {
        // Adler-32 of the SPDY/3 dictionary, as zlib reports it.
        let (mut a, mut b) = (1u32, 0u32);
        for &x in DICTIONARY {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!((b << 16) | a, 0xe3c6a7c2);
    }

    #[tokio::test]
    async fn client_and_server_forward_bytes() {
        use crate::kube::agent::hub::StreamHandle;
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        // The "pod" echoes what it gets, upper-cased.
        let open: Opener = Arc::new(|port: u16| {
            Box::pin(async move {
                assert_eq!(port, 8080);
                Ok(StreamHandle::echo_upper_for_tests())
            })
        });
        tokio::spawn(serve(server_io, open));
        let (mut user, local) = tokio::io::duplex(1024);
        let fwd = tokio::spawn(forward_one(client_io, 8080, local));
        user.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        user.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"HELLO");
        drop(user);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fwd).await;
    }

    /// client-go's SPDY executor: error, stdin, stdout and stderr streams;
    /// stdin goes to the command, its output comes back on stdout, and the
    /// v4 Status on the error stream ends it.
    #[tokio::test]
    async fn exec_over_spdy() {
        use crate::kube::agent::hub::StreamHandle;
        let (client, server) = tokio::io::duplex(64 * 1024);
        let want = ExecStreams { stdin: true, stdout: true, stderr: true, tty: false };
        let server = tokio::spawn(serve_exec(server, StreamHandle::echo_upper_for_tests(), want, true));
        let (mut rd, mut wr) = tokio::io::split(client);
        let mut headers = HeaderWriter { started: false };
        for (sid, kind) in [(1u32, "error"), (3, "stdin"), (5, "stdout"), (7, "stderr")] {
            let mut body = sid.to_be_bytes().to_vec();
            body.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            body.extend_from_slice(&headers.block(&[("streamtype", kind)]));
            wr.write_all(&control(TYPE_SYN_STREAM, 0, &body)).await.unwrap();
        }
        wr.write_all(&data(3, 0, b"abc")).await.unwrap();
        wr.write_all(&data(3, FLAG_FIN, &[])).await.unwrap();
        let (mut stdout, mut error, mut replies) = (Vec::new(), Vec::new(), 0);
        let read = async {
            loop {
                let mut head = [0u8; 8];
                rd.read_exact(&mut head).await.unwrap();
                let len = u32::from_be_bytes([0, head[5], head[6], head[7]]) as usize;
                let mut payload = vec![0u8; len];
                rd.read_exact(&mut payload).await.unwrap();
                if head[0] & 0x80 != 0 {
                    if u16::from_be_bytes([head[2], head[3]]) == TYPE_SYN_REPLY {
                        replies += 1;
                    }
                    continue;
                }
                let sid = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
                match sid {
                    5 => stdout.extend_from_slice(&payload),
                    1 => {
                        error.extend_from_slice(&payload);
                        if head[4] & FLAG_FIN != 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), read).await.expect("exec finished");
        assert_eq!(replies, 4);
        assert_eq!(stdout, b"ABC");
        let status: serde_json::Value = serde_json::from_slice(&error).unwrap();
        assert_eq!(status["status"], "Success");
        drop(wr);
        drop(rd);
        tokio::time::timeout(std::time::Duration::from_secs(10), server).await.unwrap().unwrap().unwrap();
    }

    #[test]
    fn our_header_blocks_inflate() {
        let mut w = HeaderWriter { started: false };
        let mut r = HeaderReader::new();
        let a = w.block(&[("streamtype", "data"), ("port", "80")]);
        let b = w.block(&[("requestid", "7")]);
        let h = r.block(&a).unwrap();
        assert_eq!(h.get("port").map(String::as_str), Some("80"));
        let h = r.block(&b).unwrap();
        assert_eq!(h.get("requestid").map(String::as_str), Some("7"));
    }
}
