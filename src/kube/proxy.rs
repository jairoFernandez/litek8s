//! The `proxy` subresource of pods and services
//! (`/api/v1/namespaces/<ns>/pods/<name>[:<port>]/proxy/<path>`): an HTTP
//! request relayed to a port of the pod. The bytes travel over the node
//! agent's port-forward stream, so the API server never needs a route to
//! pod addresses.

use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper::header::{CONNECTION, HOST, TRANSFER_ENCODING, UPGRADE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;

use super::controllers::util;
use super::server::{ApiError, ApiState, Body, Req, full};

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", message)
}

/// Whether the proxy URL names the `https` scheme (`https:name:port`).
fn wants_tls(raw: &str) -> bool {
    raw.starts_with("https:")
}

/// `name`, `name:port` or `scheme:name:port` as the proxy URL carries it.
fn split_name_port(raw: &str) -> (String, String) {
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.as_slice() {
        [name] => (name.to_string(), String::new()),
        [name, port] => (name.to_string(), port.to_string()),
        [_scheme, name, port, ..] => (name.to_string(), port.to_string()),
        _ => (raw.to_string(), String::new()),
    }
}

/// A container port of the pod by number or name; empty means the first
/// declared port (or 80).
fn pod_port(pod: &Value, port: &str) -> Option<u16> {
    if let Ok(p) = port.parse::<u16>() {
        return Some(p);
    }
    let ports: Vec<&Value> = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|c| c.get("ports").and_then(Value::as_array).into_iter().flatten())
        .collect();
    if port.is_empty() {
        return Some(ports.first().and_then(|p| p.get("containerPort")).and_then(Value::as_u64).unwrap_or(80) as u16);
    }
    ports
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(port))
        .and_then(|p| p.get("containerPort"))
        .and_then(Value::as_u64)
        .map(|p| p as u16)
}

pub(crate) async fn pod(state: &Arc<ApiState>, ns: &str, name_port: &str, path: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let (name, port) = split_name_port(name_port);
    let pod = state
        .get_object("", "pods", ns, &name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "NotFound", format!("pods \"{name}\" not found")))?;
    let port = pod_port(&pod, &port).ok_or_else(|| ApiError::bad_request(format!("no such port {port} on pod {name}")))?;
    relay(state, &pod, port, path, req, &format!("/api/v1/namespaces/{ns}/pods/{name_port}/proxy"), wants_tls(name_port)).await
}

pub(crate) async fn service(state: &Arc<ApiState>, ns: &str, name_port: &str, path: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let (name, port) = split_name_port(name_port);
    let svc = state
        .get_object("", "services", ns, &name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "NotFound", format!("services \"{name}\" not found")))?;
    let ports: Vec<Value> = svc.pointer("/spec/ports").and_then(Value::as_array).cloned().unwrap_or_default();
    let svc_port = if port.is_empty() {
        ports.first().cloned()
    } else {
        ports
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(port.as_str()) || p.get("port").and_then(Value::as_u64).map(|n| n.to_string()) == Some(port.clone()))
            .cloned()
    }
    .ok_or_else(|| unavailable(format!("no service port {port:?} found for service \"{name}\"")))?;
    let port_name = svc_port.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let endpoints = state.get_object("", "endpoints", ns, &name);
    for subset in endpoints.as_ref().and_then(|e| e.get("subsets")).and_then(Value::as_array).into_iter().flatten() {
        let target = subset
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|p| p.get("name").and_then(Value::as_str).unwrap_or_default() == port_name)
            .and_then(|p| p.get("port"))
            .and_then(Value::as_u64);
        let Some(target) = target else { continue };
        for addr in subset.get("addresses").and_then(Value::as_array).into_iter().flatten() {
            if addr.pointer("/targetRef/kind").and_then(Value::as_str) != Some("Pod") {
                continue;
            }
            let pod_name = util::s(addr, "/targetRef/name");
            let pod_ns = match util::s(addr, "/targetRef/namespace") {
                "" => ns,
                other => other,
            };
            if let Some(pod) = state.get_object("", "pods", pod_ns, pod_name) {
                return relay(state, &pod, target as u16, path, req, &format!("/api/v1/namespaces/{ns}/services/{name_port}/proxy"), wants_tls(name_port)).await;
            }
        }
    }
    Err(unavailable(format!("no endpoints available for service \"{name}\"")))
}

async fn relay(state: &Arc<ApiState>, pod: &Value, port: u16, path: &str, req: &Req, prefix: &str, tls: bool) -> Result<Response<Body>, ApiError> {
    let node = util::s(pod, "/spec/nodeName").to_string();
    if node.is_empty() || !(state.hub.is_connected(&node) || state.node_server_addr(&node).is_some()) {
        return Err(unavailable(format!("pod {} is not running on a connected node", util::name(pod))));
    }
    let client_side = super::admission::pod_stream(state, &node, &util::uid(pod), port)
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    let result = async {
        let io: Box<dyn Io> = if tls {
            let host = util::s(pod, "/status/podIP").to_string();
            Box::new(super::agent::probe::tls_connect_any(client_side, if host.is_empty() { "pod" } else { &host }).await?)
        } else {
            Box::new(client_side)
        };
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(conn);
        let target = if req.raw_query().is_empty() { path.to_string() } else { format!("{path}?{}", req.raw_query()) };
        let mut builder = Request::builder().method(req.method().clone()).uri(target);
        for (k, v) in req.headers() {
            if k == HOST || k == CONNECTION || k == UPGRADE || k == TRANSFER_ENCODING || k.as_str() == "authorization" {
                continue;
            }
            builder = builder.header(k, v);
        }
        builder = builder.header(HOST, format!("{}:{port}", util::s(pod, "/status/podIP")));
        let request = builder.body(http_body_util::Full::new(req.body().clone()))?;
        let response = sender.send_request(request).await?;
        let (parts, body) = response.into_parts();
        let bytes = http_body_util::Limited::new(body, MAX_RESPONSE)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_bytes();
        anyhow::Ok((parts, bytes))
    }
    .await;
    let (parts, bytes) = result.map_err(|e| unavailable(format!("error trying to reach pod: {e:#}")))?;
    // Like the API server's proxy, point absolute links in HTML back
    // through the proxy.
    let html = parts.headers.get(hyper::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("text/html"));
    let bytes = if html {
        let text = String::from_utf8_lossy(&bytes).to_string();
        let mut out = text.clone();
        for attr in ["href", "src", "action"] {
            for q in ['"', '\''] {
                out = out.replace(&format!("{attr}={q}/"), &format!("{attr}={q}{prefix}/"));
            }
        }
        if out == text { bytes } else { Bytes::from(out) }
    } else {
        bytes
    };
    let mut resp = Response::new(full(bytes));
    *resp.status_mut() = parts.status;
    for (k, v) in &parts.headers {
        if k == CONNECTION || k == TRANSFER_ENCODING || k.as_str() == "content-length" || k.as_str() == "keep-alive" {
            continue;
        }
        resp.headers_mut().append(k, v.clone());
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_ports() {
        assert_eq!(split_name_port("web"), ("web".into(), "".into()));
        assert_eq!(split_name_port("web:8080"), ("web".into(), "8080".into()));
        assert_eq!(split_name_port("http:web:80"), ("web".into(), "80".into()));
        let pod = json!({"spec": {"containers": [{"ports": [{"name": "http", "containerPort": 8080}]}]}});
        assert_eq!(pod_port(&pod, ""), Some(8080));
        assert_eq!(pod_port(&pod, "http"), Some(8080));
        assert_eq!(pod_port(&pod, "81"), Some(81));
        assert_eq!(pod_port(&pod, "nope"), None);
    }
}
