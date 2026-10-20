//! The UI's HTTP/1.1 client: one connection per request over a Unix socket
//! (the Docker API), TCP or TLS (the Kubernetes API), with upgrades for
//! exec and port-forward. Replies are kept as JSON values: the UI reads a
//! few fields and should not fail on what other engines add or leave out.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Clone)]
pub enum Transport {
    Unix(PathBuf),
    Tcp(String),
    Tls { addr: String, server_name: String, config: Option<Arc<rustls::ClientConfig>> },
}

/// Credentials that change (a kubeconfig's exec plugin): the
/// `Authorization` header for the next request.
pub trait Credentials: Send + Sync {
    fn header(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;
    /// The server refused the last one (401): get a new one next time.
    fn invalidate(&self);
}

/// Where an API is and how to authenticate to it.
#[derive(Clone)]
pub struct Endpoint {
    pub transport: Transport,
    /// `Host` header.
    pub host: String,
    /// Prefix of every path (a kubeconfig server URL may have one).
    pub base: String,
    /// Extra headers on every request (`Authorization`).
    pub headers: Vec<(String, String)>,
    /// What to show the user.
    pub display: String,
    pub creds: Option<Arc<dyn Credentials>>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the headers: they carry credentials.
        write!(f, "Endpoint({})", self.display)
    }
}

impl Endpoint {
    pub fn unix(path: PathBuf) -> Self {
        Endpoint { display: format!("unix://{}", path.display()), transport: Transport::Unix(path), host: "localhost".into(), base: String::new(), headers: Vec::new(), creds: None }
    }

    /// `unix:///path`, `tcp://host:port` or `http://host:port` (`DOCKER_HOST` syntax).
    pub fn parse_docker_host(url: &str) -> Result<Self> {
        if let Some(p) = url.strip_prefix("unix://") {
            return Ok(Self::unix(PathBuf::from(p)));
        }
        if let Some(rest) = url.strip_prefix("tcp://").or_else(|| url.strip_prefix("http://")) {
            let addr = rest.trim_end_matches('/').to_string();
            let addr = if addr.contains(':') { addr } else { format!("{addr}:2375") };
            return Ok(Endpoint { display: format!("tcp://{addr}"), host: addr.clone(), transport: Transport::Tcp(addr), base: String::new(), headers: Vec::new(), creds: None });
        }
        if url.starts_with('/') {
            return Ok(Self::unix(PathBuf::from(url)));
        }
        bail!("unsupported Docker host {url:?} (unix:// or tcp://)")
    }

    /// An `http://` or `https://` server URL.
    pub fn parse_http(url: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Result<Self> {
        let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            bail!("unsupported server URL {url:?}")
        };
        let (authority, base) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
            None => (rest, String::new()),
        };
        let default_port = if secure { 443 } else { 80 };
        let (host_only, has_port) = match authority.strip_prefix('[') {
            Some(v6) => (v6.split(']').next().unwrap_or_default().to_string(), v6.contains("]:")),
            None => (authority.split(':').next().unwrap_or_default().to_string(), authority.contains(':')),
        };
        let addr = if has_port { authority.to_string() } else { format!("{authority}:{default_port}") };
        let transport = if secure { Transport::Tls { addr: addr.clone(), server_name: host_only, config: tls } } else { Transport::Tcp(addr.clone()) };
        Ok(Endpoint { transport, host: authority.to_string(), base, headers: Vec::new(), display: url.trim_end_matches('/').to_string(), creds: None })
    }

    pub async fn connect(&self) -> Result<Box<dyn Io>> {
        let timeout = Duration::from_secs(5);
        match &self.transport {
            Transport::Unix(path) => {
                let s = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path))
                    .await
                    .context("timed out")?
                    .with_context(|| format!("cannot connect to {}", path.display()))?;
                Ok(Box::new(s))
            }
            Transport::Tcp(addr) => {
                let s = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await.context("timed out")?.with_context(|| format!("cannot connect to {addr}"))?;
                let _ = s.set_nodelay(true);
                Ok(Box::new(s))
            }
            Transport::Tls { addr, server_name, config } => {
                let tcp = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await.context("timed out")?.with_context(|| format!("cannot connect to {addr}"))?;
                let _ = tcp.set_nodelay(true);
                match config {
                    Some(config) => {
                        let name = rustls::pki_types::ServerName::try_from(server_name.clone())?;
                        Ok(Box::new(tokio_rustls::TlsConnector::from(config.clone()).connect(name, tcp).await.with_context(|| format!("TLS handshake with {addr} failed"))?))
                    }
                    None => Ok(Box::new(crate::kube::agent::probe::tls_connect_any(tcp, server_name).await?)),
                }
            }
        }
    }

    async fn request(&self, method: Method, path: &str, content_type: Option<&str>, body: Vec<u8>) -> Result<Request<Full<Bytes>>> {
        let mut req = Request::builder().method(method).uri(format!("{}{path}", self.base)).header(hyper::header::HOST, &self.host);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(c) = &self.creds {
            req = req.header(hyper::header::AUTHORIZATION, c.header().await?);
        }
        if let Some(ct) = content_type {
            req = req.header(hyper::header::CONTENT_TYPE, ct);
        }
        Ok(req.body(Full::new(Bytes::from(body)))?)
    }

    /// Send a request; the response with its body still streaming.
    pub async fn send(&self, method: Method, path: &str, content_type: Option<&str>, body: Vec<u8>) -> Result<Response<Incoming>> {
        let io = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let resp = sender.send_request(self.request(method, path, content_type, body).await?).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            if let Some(c) = &self.creds {
                c.invalidate();
            }
        }
        Ok(resp)
    }

    /// Like [`Endpoint::send`] with more headers (`Accept`).
    pub async fn send_with(&self, method: Method, path: &str, headers: &[(&str, &str)], content_type: Option<&str>, body: Vec<u8>) -> Result<Response<Incoming>> {
        let io = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = self.request(method, path, content_type, body).await?;
        for (k, v) in headers {
            req.headers_mut().insert(hyper::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
        }
        let resp = sender.send_request(req).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            if let Some(c) = &self.creds {
                c.invalidate();
            }
        }
        Ok(resp)
    }

    /// A request whose reply is JSON (`null` for an empty body); an error
    /// status becomes an error with the server's message.
    pub async fn json(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        self.json_typed(method, path, "application/json", body).await
    }

    pub async fn json_typed(&self, method: Method, path: &str, content_type: &str, body: Option<&Value>) -> Result<Value> {
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default()).unwrap_or_default();
        let ct = body.map(|_| content_type);
        let resp = tokio::time::timeout(Duration::from_secs(30), self.send(method, path, ct, bytes)).await.context("timed out")??;
        let status = resp.status();
        let data = resp.into_body().collect().await?.to_bytes();
        check(status, &data)?;
        if data.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&data).with_context(|| format!("{path}: not JSON"))?)
    }

    /// A GET whose reply is JSON, with an `Accept` (Tables, discovery),
    /// read into `T` (typed lists keep only the fields they name).
    pub async fn json_accept<T: serde::de::DeserializeOwned>(&self, path: &str, accept: &str) -> Result<T> {
        let resp = tokio::time::timeout(Duration::from_secs(30), self.send_with(Method::GET, path, &[("Accept", accept)], None, Vec::new())).await.context("timed out")??;
        let status = resp.status();
        let data = resp.into_body().collect().await?.to_bytes();
        check(status, &data)?;
        Ok(serde_json::from_slice(&data).with_context(|| format!("{path}: not JSON"))?)
    }

    /// An upgraded connection (`Upgrade: <protocol>`): exec, port-forward.
    pub async fn upgrade(&self, method: Method, path: &str, protocol: &str, extra: &[(&str, &str)], body: Option<&Value>) -> Result<(Response<()>, Pin<Box<dyn Io>>)> {
        let io = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default()).unwrap_or_default();
        let mut req = self.request(method, path, body.map(|_| "application/json"), bytes).await?;
        let h = req.headers_mut();
        h.insert(hyper::header::CONNECTION, "Upgrade".parse()?);
        h.insert(hyper::header::UPGRADE, protocol.parse()?);
        for (k, v) in extra {
            h.insert(hyper::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
        }
        let mut resp = sender.send_request(req).await?;
        let status = resp.status();
        if status != StatusCode::SWITCHING_PROTOCOLS && !status.is_success() {
            let data = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            check(status, &data)?;
            bail!("{status}");
        }
        let upgraded = hyper::upgrade::on(&mut resp).await.context("upgrade failed")?;
        let (parts, _) = resp.into_parts();
        Ok((Response::from_parts(parts, ()), Box::pin(TokioIo::new(upgraded))))
    }
}

/// An error for a non-2xx status, with the message Docker (`message`) or
/// Kubernetes (`message` of a Status) put in the body.
pub fn check(status: StatusCode, body: &[u8]) -> Result<()> {
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        return Ok(());
    }
    let msg = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    if msg.is_empty() { bail!("{status}") } else { bail!("{msg}") }
}

/// Splits a streaming body into lines (watch events, Docker events, logs).
pub struct Lines {
    body: Incoming,
    buf: Vec<u8>,
}

impl Lines {
    pub fn new(body: Incoming) -> Self {
        Lines { body, buf: Vec::new() }
    }

    /// The next line without its newline; `None` at the end.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop();
                return Some(line);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                _ => {
                    if self.buf.is_empty() {
                        return None;
                    }
                    return Some(std::mem::take(&mut self.buf));
                }
            }
        }
    }

    /// The next chunk of raw bytes (whatever is buffered first).
    pub async fn chunk(&mut self) -> Option<Vec<u8>> {
        if !self.buf.is_empty() {
            return Some(std::mem::take(&mut self.buf));
        }
        loop {
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if !data.is_empty() {
                            return Some(data.to_vec());
                        }
                    }
                }
                _ => return None,
            }
        }
    }
}

/// Percent-encode a query value.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        let e = Endpoint::parse_docker_host("unix:///tmp/d.sock").unwrap();
        assert!(matches!(e.transport, Transport::Unix(ref p) if p == std::path::Path::new("/tmp/d.sock")));
        let e = Endpoint::parse_docker_host("tcp://10.0.0.2").unwrap();
        assert!(matches!(e.transport, Transport::Tcp(ref a) if a == "10.0.0.2:2375"));
        assert!(Endpoint::parse_docker_host("ssh://me@box").is_err());

        let e = Endpoint::parse_http("http://127.0.0.1:6443", None).unwrap();
        assert!(matches!(e.transport, Transport::Tcp(ref a) if a == "127.0.0.1:6443"));
        assert_eq!(e.base, "");
        let e = Endpoint::parse_http("https://k8s.example:8443/prefix/", None).unwrap();
        match &e.transport {
            Transport::Tls { addr, server_name, .. } => {
                assert_eq!(addr, "k8s.example:8443");
                assert_eq!(server_name, "k8s.example");
            }
            _ => panic!("not TLS"),
        }
        assert_eq!(e.base, "/prefix");
        let e = Endpoint::parse_http("https://k8s.example", None).unwrap();
        assert!(matches!(e.transport, Transport::Tls { ref addr, .. } if addr == "k8s.example:443"));
        let e = Endpoint::parse_http("https://[::1]:6443", None).unwrap();
        assert!(matches!(e.transport, Transport::Tls { ref addr, ref server_name, .. } if addr == "[::1]:6443" && server_name == "::1"));
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(enc(r#"{"label":["a=b"]}"#), "%7B%22label%22%3A%5B%22a%3Db%22%5D%7D");
    }
}
