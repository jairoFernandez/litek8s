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
    /// The pod's sandbox (busybox, owns the network namespace).
    pub sandbox_id: Option<String>,
    pub host_network: bool,
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

/// Run a lifecycle hook handler (`postStart`/`preStop`: exec, httpGet,
/// tcpSocket or sleep); true when it succeeded.
pub(crate) async fn run_handler<R: PodRuntime>(rt: &R, handler: &Value, target: &ProbeTarget) -> bool {
    if let Some(seconds) = handler.pointer("/sleep/seconds").and_then(Value::as_u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        return true;
    }
    check(rt, handler, target).await
}

async fn check<R: PodRuntime>(rt: &R, probe: &Value, target: &ProbeTarget) -> bool {
    if let Some(command) = probe.pointer("/exec/command").and_then(Value::as_array) {
        let command: Vec<String> = command.iter().filter_map(Value::as_str).map(str::to_string).collect();
        return exec_ok(rt, &target.container_id, command).await;
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
        return match dial(host, port, target).await {
            Dial::Direct(tcp) => http_get(tcp, host, port, path, &headers, https).await,
            Dial::Refused => false,
            Dial::InPod(sandbox, remember) => {
                let scheme = if https { "https" } else { "http" };
                let mut command = vec!["wget".to_string(), "-q".into(), "-O".into(), "/dev/null".into(), "-T".into(), "1".into()];
                if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("User-Agent")) {
                    command.push("--header".into());
                    command.push("User-Agent: kube-probe/1.33".into());
                }
                for (k, v) in &headers {
                    command.push("--header".into());
                    command.push(format!("{k}: {v}"));
                }
                if https {
                    command.push("--no-check-certificate".into());
                }
                command.push(format!("{scheme}://{}{path}", host_port(host, port)));
                in_pod_ok(rt, &sandbox, command, remember).await
            }
        };
    }
    let port = probe
        .pointer("/tcpSocket/port")
        .or_else(|| probe.pointer("/grpc/port"));
    if let Some(port) = resolve_port(port, target) {
        let host = probe.pointer("/tcpSocket/host").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or(&target.host);
        return match dial(host, port, target).await {
            Dial::Direct(_) => true,
            Dial::Refused => false,
            // `-e true`: connect, then exit at once with success.
            Dial::InPod(sandbox, remember) => {
                in_pod_ok(rt, &sandbox, vec!["nc".into(), host.into(), port.to_string(), "-e".into(), "true".into()], remember).await
            }
        };
    }
    false
}

enum Dial {
    Direct(TcpStream),
    Refused,
    /// The node has no working path to the pod address: probe from inside
    /// the pod's network namespace, through its sandbox. If the second
    /// field is set, remember it as unroutable once that works.
    InPod(String, Option<String>),
}

/// Pod addresses (or `address:port`s) the node could not reach directly.
/// With Docker in a VM the host's path to a container address can be
/// missing (Docker Desktop) or stay stale for a new address (OrbStack)
/// although the pod is fine, and a hostNetwork pod's loopback is the VM's,
/// not the node's; probes to these go through the pod's sandbox instead of
/// failing or waiting for a timeout each period.
static UNROUTABLE: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);

/// How long a direct connect may take before the address counts as
/// unroutable. A listening socket on a local bridge answers in well under a
/// millisecond; a missing route never answers.
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// Whether the node failed to reach `addr` (an address, or `address:port`).
pub(crate) fn is_unroutable(addr: &str) -> bool {
    UNROUTABLE.lock().unwrap().as_ref().is_some_and(|s| s.contains(addr))
}

pub(crate) fn mark_unroutable(addr: &str) {
    UNROUTABLE.lock().unwrap().get_or_insert_default().insert(addr.to_string());
}

async fn dial(host: &str, port: u16, target: &ProbeTarget) -> Dial {
    let Some(sandbox) = target.sandbox_id.clone() else {
        return TcpStream::connect((host, port)).await.map_or(Dial::Refused, Dial::Direct);
    };
    let host_port = host_port(host, port);
    if is_unroutable(host) || is_unroutable(&host_port) {
        return Dial::InPod(sandbox, None);
    }
    match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(Ok(tcp)) => Dial::Direct(tcp),
        // On a hostNetwork pod, a refusal may only mean the process listens
        // on a loopback the node does not share (Docker in a VM).
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused && target.host_network => Dial::InPod(sandbox, Some(host_port)),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Dial::Refused,
        _ => {
            mark_unroutable(host);
            Dial::InPod(sandbox, None)
        }
    }
}

async fn in_pod_ok<R: PodRuntime>(rt: &R, sandbox: &str, command: Vec<String>, remember: Option<String>) -> bool {
    let ok = exec_ok(rt, sandbox, command).await;
    if let Some(key) = remember.filter(|_| ok) {
        mark_unroutable(&key);
    }
    ok
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

/// Run `command` in container `id`; true when it exits 0.
async fn exec_ok<R: PodRuntime>(rt: &R, id: &str, command: Vec<String>) -> bool {
    let Ok(session) = rt.exec(id, command, false, false).await else {
        return false;
    };
    let mut output = session.output;
    while output.next().await.is_some() {}
    rt.exec_exit_code(&session.id).await.ok().flatten() == Some(0)
}

/// A probe-style GET: success is any 2xx/3xx status.
async fn http_get(tcp: TcpStream, host: &str, port: u16, path: &str, headers: &[(String, String)], https: bool) -> bool {
    // Probe headers replace the defaults of the same name, as the kubelet's do.
    let mut all: Vec<(String, String)> = vec![
        ("Host".into(), host_port(host, port)),
        ("User-Agent".into(), "kube-probe/1.33".into()),
        ("Accept".into(), "*/*".into()),
    ];
    all.retain(|(k, _)| !headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k)));
    all.extend(headers.iter().cloned());
    let mut request = format!("GET {path} HTTP/1.1\r\n");
    for (k, v) in &all {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("Connection: close\r\n");
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
