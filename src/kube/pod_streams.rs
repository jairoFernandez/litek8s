//! Pod subresources that stream: `log` (plain HTTP, optionally following)
//! and `exec`/`attach` (WebSocket, `v5.channel.k8s.io`). The bytes come from
//! the pod's node agent over its persistent connection (see `agent::hub`).

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{CONNECTION, CONTENT_TYPE, HeaderValue, UPGRADE};
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use super::agent::hub::{StreamEvent, StreamHandle};
use super::agent::protocol::Message;
use super::controllers::util;
use super::server::{ApiError, ApiState, Body, Req};
use super::ws;

/// The pod, its node and the container a request targets.
fn target(state: &ApiState, ns: &str, name: &str, req: &Req) -> Result<(Value, String, String), ApiError> {
    let pod = state
        .get_object("", "pods", ns, name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "NotFound", format!("pods \"{name}\" not found")))?;
    let node = util::s(&pod, "/spec/nodeName").to_string();
    if node.is_empty() {
        return Err(ApiError::bad_request(format!("pod {name} does not have a host assigned")));
    }
    let names: Vec<String> = ["/spec/containers", "/spec/initContainers", "/spec/ephemeralContainers"]
        .iter()
        .filter_map(|p| pod.pointer(p).and_then(Value::as_array))
        .flatten()
        .map(|c| util::s(c, "/name").to_string())
        .collect();
    let main: Vec<&String> = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map(|cs| cs.iter().filter_map(|c| c.get("name").and_then(Value::as_str)).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| names.iter().find(|x| *x == n))
        .collect();
    let container = match req.q("container") {
        Some(c) if names.iter().any(|n| n == c) => c.to_string(),
        Some(c) => return Err(ApiError::bad_request(format!("container {c} is not valid for pod {name}"))),
        None if main.len() == 1 => main[0].clone(),
        None => {
            let choices: Vec<&str> = main.iter().map(|s| s.as_str()).collect();
            return Err(ApiError::bad_request(format!(
                "a container name must be specified for pod {name}, choose one of: [{}]",
                choices.join(" ")
            )));
        }
    };
    if !state.hub.is_connected(&node) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            format!("node \"{node}\" is not connected"),
        ));
    }
    Ok((pod, node, container))
}

fn open_error(e: anyhow::Error) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", format!("{e:#}"))
}

pub(crate) async fn log(state: &Arc<ApiState>, ns: &str, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let (pod, node, container) = target(state, ns, name, req)?;
    let uid = util::uid(&pod);
    let truthy = |k: &str| matches!(req.q(k), Some("true" | "1"));
    let num = |k: &str| req.q(k).and_then(|v| v.parse::<i64>().ok());
    let (follow, previous, timestamps) = (truthy("follow"), truthy("previous"), truthy("timestamps"));
    let (tail_lines, since_seconds, limit_bytes) = (num("tailLines"), num("sinceSeconds"), num("limitBytes"));
    let mut handle = state
        .hub
        .open_stream(&node, |stream| Message::Logs {
            stream,
            pod_uid: uid,
            container,
            follow,
            previous,
            timestamps,
            tail_lines,
            since_seconds,
            limit_bytes,
        })
        .await
        .map_err(open_error)?;
    // Wait for the first event so a missing container is a proper error.
    let first = match tokio::time::timeout(Duration::from_secs(30), handle.events.recv()).await {
        Ok(Some(e)) => Some(e),
        Ok(None) => None,
        Err(_) if follow => None,
        Err(_) => return Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "Timeout", "timed out reading logs")),
    };
    if let Some(StreamEvent::Done { error: Some(error), .. }) = &first {
        return Err(ApiError::bad_request(error.clone()));
    }
    // Logs over a WebSocket (`binary.k8s.io`, or base64 text frames).
    if req.header(UPGRADE.as_str()).eq_ignore_ascii_case("websocket") {
        let offered = req.header("sec-websocket-protocol").to_string();
        let base64 = offered.split(',').any(|p| p.trim() == "base64.binary.k8s.io") && !offered.split(',').any(|p| p.trim() == "binary.k8s.io");
        let protocol = if base64 { "base64.binary.k8s.io" } else { "binary.k8s.io" };
        let key = req.header("sec-websocket-key").to_string();
        let on_upgrade = req.take_upgrade().ok_or_else(|| ApiError::bad_request("connection cannot be upgraded"))?;
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else { return };
            let (_reader, mut writer) = ws::split(TokioIo::new(upgraded));
            let mut pending = first;
            loop {
                let event = match pending.take() {
                    Some(e) => Some(e),
                    None => handle.events.recv().await,
                };
                let Some(StreamEvent::Data { bytes, .. }) = event else { break };
                let sent = if base64 {
                    use base64::Engine;
                    writer.text(base64::engine::general_purpose::STANDARD.encode(&bytes).as_bytes()).await
                } else {
                    writer.binary(&bytes).await
                };
                if sent.is_err() {
                    break;
                }
            }
            let _ = writer.close().await;
        });
        let mut resp = Response::new(super::server::full(Bytes::new()));
        *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let h = resp.headers_mut();
        h.insert(UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        h.insert("sec-websocket-accept", HeaderValue::from_str(&ws::accept_key(&key)).unwrap());
        if !offered.is_empty() {
            h.insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
        }
        return Ok(resp);
    }
    let body = stream::unfold((handle, first), |(mut handle, pending): (StreamHandle, Option<StreamEvent>)| async move {
        let event = match pending {
            Some(e) => Some(e),
            None => handle.events.recv().await,
        };
        match event {
            Some(StreamEvent::Data { bytes, .. }) => Some((Ok::<_, std::io::Error>(Frame::data(bytes)), (handle, None))),
            _ => None,
        }
    });
    let mut resp = Response::new(StreamBody::new(body).boxed_unsync());
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    Ok(resp)
}

/// `exec` and `attach` over WebSocket. kubectl tries this first and falls
/// back to SPDY, which is not served.
pub(crate) async fn exec(state: &Arc<ApiState>, ns: &str, name: &str, req: &Req, attach: bool) -> Result<Response<Body>, ApiError> {
    let (pod, node, container) = target(state, ns, name, req)?;
    let upgrade = req.header(UPGRADE.as_str()).to_ascii_lowercase();
    if upgrade != "websocket" {
        return Err(ApiError::bad_request("streaming requires a WebSocket upgrade (SPDY is not supported)"));
    }
    let offered: Vec<String> = req
        .header("sec-websocket-protocol")
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let protocol = ["v5.channel.k8s.io", "v4.channel.k8s.io", "channel.k8s.io"]
        .into_iter()
        .find(|p| offered.iter().any(|o| o == p))
        .ok_or_else(|| ApiError::bad_request("no supported streaming protocol offered (want v5.channel.k8s.io or v4.channel.k8s.io)"))?;
    let key = req.header("sec-websocket-key");
    if key.is_empty() {
        return Err(ApiError::bad_request("missing Sec-WebSocket-Key"));
    }
    let truthy = |k: &str| matches!(req.q(k), Some("true" | "1"));
    let command: Vec<String> = req.query_all("command");
    if !attach && command.is_empty() {
        return Err(ApiError::bad_request("you must specify at least 1 command"));
    }
    let (stdin, tty) = (truthy("stdin"), truthy("tty"));
    let on_upgrade = req.take_upgrade().ok_or_else(|| ApiError::bad_request("connection cannot be upgraded"))?;
    let uid = util::uid(&pod);
    let command = if attach { Vec::new() } else { command };
    let handle = state
        .hub
        .open_stream(&node, |stream| Message::Exec { stream, pod_uid: uid, container, command, stdin, tty })
        .await
        .map_err(open_error)?;
    let v5 = protocol == "v5.channel.k8s.io";
    let v1 = protocol == "channel.k8s.io";
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else { return };
        bridge(TokioIo::new(upgraded), handle, v5, v1).await;
    });
    let mut resp = Response::new(super::server::full(Bytes::new()));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert("sec-websocket-accept", HeaderValue::from_str(&ws::accept_key(key)).unwrap());
    h.insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
    Ok(resp)
}

/// Shuttle bytes between the WebSocket (channel-prefixed messages) and the
/// node's exec stream.
async fn bridge<S>(socket: S, mut handle: StreamHandle, v5: bool, v1: bool)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = ws::split(socket);
    let input = handle.input();
    let reader_task = tokio::spawn(async move {
        loop {
            match reader.next().await {
                Ok(ws::Incoming::Binary(msg)) | Ok(ws::Incoming::Text(msg)) => {
                    let Some((&channel, data)) = msg.split_first() else { continue };
                    match channel {
                        0 => {
                            if !input.send(0, Bytes::copy_from_slice(data)).await {
                                break;
                            }
                        }
                        4 => {
                            if let Ok(size) = serde_json::from_slice::<Value>(data) {
                                let w = size.get("Width").and_then(Value::as_u64).unwrap_or(80) as u16;
                                let h = size.get("Height").and_then(Value::as_u64).unwrap_or(24) as u16;
                                input.resize(w, h).await;
                            }
                        }
                        255 if v5 => {
                            if data.first() == Some(&0) {
                                input.close().await;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(ws::Incoming::Close) | Err(_) => break,
            }
        }
        input.close().await;
    });
    while let Some(event) = handle.events.recv().await {
        match event {
            StreamEvent::Data { channel, bytes } => {
                let mut msg = Vec::with_capacity(bytes.len() + 1);
                msg.push(if channel == 2 { 2 } else { 1 });
                msg.extend_from_slice(&bytes);
                if writer.binary(&msg).await.is_err() {
                    break;
                }
            }
            StreamEvent::Done { exit_code, error } => {
                let status = match (exit_code, error) {
                    (_, Some(error)) => json!({"metadata": {}, "status": "Failure", "message": error, "reason": "InternalError", "code": 500}),
                    (Some(0) | None, None) => json!({"metadata": {}, "status": "Success"}),
                    (Some(code), None) => json!({
                        "metadata": {},
                        "status": "Failure",
                        "message": format!("command terminated with non-zero exit code: exit status {code}"),
                        "reason": "NonZeroExitCode",
                        "details": {"causes": [{"reason": "ExitCode", "message": code.to_string()}]},
                    }),
                };
                // channel.k8s.io (v1) carries an error text, nothing on success.
                if v1 {
                    if status["status"] != "Success" {
                        let mut msg = vec![3u8];
                        msg.extend_from_slice(status["message"].as_str().unwrap_or_default().as_bytes());
                        let _ = writer.binary(&msg).await;
                    }
                    break;
                }
                let mut msg = vec![3u8];
                msg.extend_from_slice(status.to_string().as_bytes());
                let _ = writer.binary(&msg).await;
                break;
            }
        }
    }
    let _ = writer.close().await;
    reader_task.abort();
}

/// `kubectl port-forward`: SPDY, tunneled through a WebSocket or over a
/// plain `SPDY/3.1` upgrade.
pub(crate) async fn port_forward(state: &Arc<ApiState>, ns: &str, name: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let pod = state
        .get_object("", "pods", ns, name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "NotFound", format!("pods \"{name}\" not found")))?;
    let node = util::s(&pod, "/spec/nodeName").to_string();
    if node.is_empty() || !state.hub.is_connected(&node) {
        return Err(ApiError::bad_request(format!("pod {name} is not running on a connected node")));
    }
    let uid = util::uid(&pod);
    let hub = state.hub.clone();
    let open: super::spdy::Opener = Arc::new(move |port| {
        let hub = hub.clone();
        let node = node.clone();
        let uid = uid.clone();
        Box::pin(async move { hub.open_stream(&node, |stream| Message::PortForward { stream, pod_uid: uid, port }).await })
    });
    let upgrade = req.header(UPGRADE.as_str()).to_ascii_lowercase();
    let on_upgrade = req.take_upgrade().ok_or_else(|| ApiError::bad_request("port-forward requires a connection upgrade"))?;
    let mut resp = Response::new(super::server::full(Bytes::new()));
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    if upgrade == "websocket" {
        const TUNNEL: &str = "SPDY/3.1+portforward.k8s.io";
        let offered = req.header("sec-websocket-protocol");
        if !offered.split(',').any(|p| p.trim() == TUNNEL) {
            return Err(ApiError::bad_request(format!("port-forward over WebSocket needs the {TUNNEL} subprotocol")));
        }
        let key = req.header("sec-websocket-key");
        let h = resp.headers_mut();
        h.insert(UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        h.insert("sec-websocket-accept", HeaderValue::from_str(&ws::accept_key(key)).unwrap());
        h.insert("sec-websocket-protocol", HeaderValue::from_static(TUNNEL));
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else { return };
            let (mut reader, mut writer) = ws::split(TokioIo::new(upgraded));
            // The SPDY byte stream is carried in binary messages.
            let (spdy_side, tunnel_side) = tokio::io::duplex(256 * 1024);
            let (mut from_spdy, mut to_spdy) = tokio::io::split(tunnel_side);
            let inbound = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Ok(ws::Incoming::Binary(m) | ws::Incoming::Text(m)) = reader.next().await {
                    if to_spdy.write_all(&m).await.is_err() {
                        break;
                    }
                }
                let _ = to_spdy.shutdown().await;
            });
            let outbound = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match from_spdy.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if writer.binary(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = writer.close().await;
            });
            let _ = super::spdy::serve(spdy_side, open).await;
            inbound.abort();
            outbound.abort();
        });
    } else if upgrade.starts_with("spdy/3.1") {
        let h = resp.headers_mut();
        h.insert(UPGRADE, HeaderValue::from_static("SPDY/3.1"));
        h.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        h.insert("x-stream-protocol-version", HeaderValue::from_static("portforward.k8s.io"));
        tokio::spawn(async move {
            let Ok(upgraded) = on_upgrade.await else { return };
            let _ = super::spdy::serve(TokioIo::new(upgraded), open).await;
        });
    } else {
        return Err(ApiError::bad_request("port-forward requires a WebSocket or SPDY/3.1 upgrade"));
    }
    Ok(resp)
}
