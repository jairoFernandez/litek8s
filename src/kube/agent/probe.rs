//! Container probes (startup, readiness, liveness): exec, httpGet,
//! tcpSocket and grpc (checked as a TCP connect).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use super::pod::PodMsg;
use super::runtime::PodRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProbeKind {
    Startup,
    Readiness,
    Liveness,
}

#[derive(Debug)]
pub(crate) struct ProbeResult {
    pub container: String,
    /// Which container instance this was for (stale results are ignored).
    pub container_id: String,
    pub kind: ProbeKind,
    pub ok: bool,
}

pub(crate) struct ProbeTarget {
    pub container: String,
    pub container_id: String,
    /// Where network probes connect (the pod IP, or loopback for hostNetwork).
    pub host: String,
    /// The container's named ports.
    pub ports: Vec<(String, u16)>,
}

fn secs(probe: &Value, field: &str, default: u64) -> u64 {
    probe.get(field).and_then(Value::as_u64).unwrap_or(default)
}

fn resolve_port(port: Option<&Value>, target: &ProbeTarget) -> Option<u16> {
    match port? {
        Value::Number(n) => n.as_u64().map(|p| p as u16),
        Value::String(name) => name
            .parse()
            .ok()
            .or_else(|| target.ports.iter().find(|(n, _)| n == name).map(|(_, p)| *p)),
        _ => None,
    }
}

/// Run one probe for the life of a container instance, reporting each
/// result. Readiness and liveness wait for `started` (the startup probe).
pub(crate) async fn run<R: PodRuntime>(
    rt: Arc<R>,
    kind: ProbeKind,
    probe: Value,
    target: ProbeTarget,
    mut started: watch::Receiver<bool>,
    tx: mpsc::UnboundedSender<PodMsg>,
) {
    if kind != ProbeKind::Startup {
        while !*started.borrow() {
            if started.changed().await.is_err() {
                return;
            }
        }
    }
    tokio::time::sleep(Duration::from_secs(secs(&probe, "initialDelaySeconds", 0))).await;
    let period = Duration::from_secs(secs(&probe, "periodSeconds", 10).max(1));
    let timeout = Duration::from_secs(secs(&probe, "timeoutSeconds", 1).max(1));
    // Readiness checks start fast (25 ms, doubling up to the period) until
    // the first success, so a pod turns Ready as soon as its process listens
    // instead of one full period later. A readiness failure before the first
    // success changes nothing (the container is not ready yet). Startup and
    // liveness failures count toward a restart, so they keep the period.
    let mut fast = kind == ProbeKind::Readiness;
    let mut wait = Duration::from_millis(25).min(period);
    let mut first = true;
    loop {
        if !first {
            tokio::time::sleep(if fast { wait } else { period }).await;
            wait = (wait * 2).min(period);
        }
        first = false;
        let ok = tokio::time::timeout(timeout, check(&*rt, &probe, &target)).await.unwrap_or(false);
        let result = ProbeResult { container: target.container.clone(), container_id: target.container_id.clone(), kind, ok };
        if ok {
            fast = false;
        }
        if tx.send(PodMsg::Probe(result)).is_err() {
            return;
        }
    }
}

async fn check<R: PodRuntime>(rt: &R, probe: &Value, target: &ProbeTarget) -> bool {
    if let Some(command) = probe.pointer("/exec/command").and_then(Value::as_array) {
        let command: Vec<String> = command.iter().filter_map(Value::as_str).map(str::to_string).collect();
        let Ok(session) = rt.exec(&target.container_id, command, false, false).await else {
            return false;
        };
        let mut output = session.output;
        while output.next().await.is_some() {}
        return rt.exec_exit_code(&session.id).await.ok().flatten() == Some(0);
    }
    if let Some(http) = probe.get("httpGet") {
        let Some(port) = resolve_port(http.get("port"), target) else { return false };
        let host = http.get("host").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or(&target.host);
        let path = http.get("path").and_then(Value::as_str).unwrap_or("/");
        let headers: Vec<(String, String)> = http
            .get("httpHeaders")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .map(|h| {
                        (
                            h.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                            h.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let https = http.get("scheme").and_then(Value::as_str) == Some("HTTPS");
        return http_get(host, port, path, &headers, https).await;
    }
    let port = probe
        .pointer("/tcpSocket/port")
        .or_else(|| probe.pointer("/grpc/port"));
    if let Some(port) = resolve_port(port, target) {
        let host = probe.pointer("/tcpSocket/host").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or(&target.host);
        return TcpStream::connect((host, port)).await.is_ok();
    }
    false
}

/// A probe-style GET: success is any 2xx/3xx status.
async fn http_get(host: &str, port: u16, path: &str, headers: &[(String, String)], https: bool) -> bool {
    let Ok(tcp) = TcpStream::connect((host, port)).await else { return false };
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: kube-probe/1.33\r\nAccept: */*\r\nConnection: close\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    let mut head = vec![0u8; 64];
    let read = if https {
        let Ok(mut tls) = tls_connect(tcp, host).await else { return false };
        if tls.write_all(request.as_bytes()).await.is_err() {
            return false;
        }
        read_status_line(&mut tls, &mut head).await
    } else {
        let mut tcp = tcp;
        if tcp.write_all(request.as_bytes()).await.is_err() {
            return false;
        }
        read_status_line(&mut tcp, &mut head).await
    };
    let Some(n) = read else { return false };
    let line = String::from_utf8_lossy(&head[..n]);
    let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (200..400).contains(&code)
}

async fn read_status_line<S: AsyncReadExt + Unpin>(s: &mut S, buf: &mut [u8]) -> Option<usize> {
    let mut n = 0;
    while n < 12 {
        match s.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => break,
            Ok(m) => n += m,
        }
    }
    (n >= 12).then_some(n)
}

/// Kubernetes does not verify certificates of HTTPS probes; neither do we.
async fn tls_connect(tcp: TcpStream, host: &str) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct AnyCert(Arc<rustls::crypto::CryptoProvider>);
    impl ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(std::io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string()).map_err(std::io::Error::other)?;
    tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await
}
