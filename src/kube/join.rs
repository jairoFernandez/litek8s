//! Joining nodes to the cluster.
//!
//! The API server keeps one join secret (in `<data-dir>/join-token`) and
//! prints a token for it, in the same shape as k3s's:
//!
//! ```text
//! K10<sha256 of the cluster CA PEM>::<secret>
//! ```
//!
//! An agent started with `litek8s agent --join https://<server>:6443 --token
//! <token>` fetches the CA from `/cacerts`, checks it against the hash in the
//! token, and from then on talks TLS pinned to that CA. It joins once
//! (`POST /apis/agent.litek8s.dev/v1/join`), which registers the node, checks
//! its node password and hands out the node's pod CIDR, and then opens the
//! persistent agent connection with the same credentials.
//!
//! Each node keeps a random node password; the server stores its hash in a
//! Secret on first join, so another machine with the token cannot take over
//! an existing node's name (it has to be deleted first, as in k3s).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::controllers::util;
use super::server::{ApiError, ApiState};

const TOKEN_PREFIX: &str = "K10";
/// Header carrying the node password on join and connect.
pub const PASSWORD_HEADER: &str = "x-litek8s-node-password";
const PASSWORD_SUFFIX: &str = ".node-password.litek8s";
pub const SERVICE_CIDR: &str = "10.43.0.0/16";

/// The cluster's pod network: each node gets one `/node_mask` of `cluster`.
#[derive(Debug, Clone)]
pub struct ClusterNetwork {
    pub cluster_cidr: String,
    pub node_mask: u8,
}

impl Default for ClusterNetwork {
    fn default() -> Self {
        Self { cluster_cidr: "10.42.0.0/16".into(), node_mask: 24 }
    }
}

/// A parsed join token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Hex sha256 of the CA PEM, when the token pins one.
    pub ca_hash: Option<String>,
    pub secret: String,
}

impl Token {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let Some(rest) = raw.strip_prefix(TOKEN_PREFIX).filter(|r| r.contains("::")) else {
            if raw.is_empty() {
                bail!("empty join token");
            }
            return Ok(Self { ca_hash: None, secret: raw.to_string() });
        };
        let (hash, secret) = rest.split_once("::").unwrap_or_default();
        // k3s tokens carry `server:<password>`; the password is the secret.
        let secret = secret.strip_prefix("server:").unwrap_or(secret);
        if secret.is_empty() {
            bail!("join token has no secret after `::`");
        }
        if !hash.is_empty() && (hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit())) {
            bail!("join token: the CA hash must be 64 hex digits");
        }
        Ok(Self { ca_hash: (!hash.is_empty()).then(|| hash.to_lowercase()), secret: secret.to_string() })
    }

    pub fn format(ca_pem: Option<&str>, secret: &str) -> String {
        format!("{TOKEN_PREFIX}{}::{secret}", ca_pem.map(sha256_hex).unwrap_or_default())
    }
}

pub fn sha256_hex(data: &str) -> String {
    hex::encode(Sha256::digest(data.as_bytes()))
}

fn random_hex(bytes: usize) -> String {
    use rand_core::RngCore;
    let mut buf = vec![0u8; bytes];
    rand_core::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Read a secret kept in `path`, or make one (0600).
fn load_or_create(path: &Path, bytes: usize) -> Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let fresh = random_hex(bytes);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, &fresh).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(fresh)
}

/// The server's join secret: `explicit`, else the stored one, else a new
/// one (kept in the data directory when there is one).
pub fn server_secret(data_dir: Option<&Path>, explicit: Option<&str>) -> Result<String> {
    if let Some(token) = explicit {
        return Ok(Token::parse(token)?.secret);
    }
    match data_dir {
        // The file holds the whole token once the server has written it.
        Some(dir) => Ok(Token::parse(&load_or_create(&dir.join("join-token"), 16)?)?.secret),
        None => Ok(random_hex(16)),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(req_auth: &str) -> &str {
    req_auth.strip_prefix("Bearer ").or_else(|| req_auth.strip_prefix("bearer ")).unwrap_or_default().trim()
}

/// Check a join/connect request's credentials for `node`.
pub(crate) fn authorize(api: &ApiState, authorization: &str, node: &str, password: &str) -> Result<(), ApiError> {
    use hyper::StatusCode;
    let presented = bearer(authorization);
    let presented = Token::parse(presented).map(|t| t.secret).unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), api.join_secret().as_bytes()) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized", "invalid join token"));
    }
    if !valid_node_name(node) {
        return Err(ApiError::bad_request(format!("invalid node name {node:?}")));
    }
    if password.len() < 16 {
        return Err(ApiError::bad_request("missing node password"));
    }
    check_node_password(api, node, password)
        .map_err(|message| ApiError::new(StatusCode::FORBIDDEN, "Forbidden", message))
}

fn valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.'])
}

/// The first join of a node records its password; later ones must match,
/// unless the Node was deleted in between (the name is free again).
fn check_node_password(api: &ApiState, node: &str, password: &str) -> Result<(), String> {
    let name = format!("{node}{PASSWORD_SUFFIX}");
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(password.as_bytes()));
    let stored = api.get_object("", "secrets", "kube-system", &name);
    let node_exists = api.get_object("", "nodes", "", node).is_some();
    if let Some(secret) = &stored {
        let current = util::s(secret, "/data/hash");
        if constant_time_eq(current.as_bytes(), hash.as_bytes()) {
            return Ok(());
        }
        if node_exists {
            return Err(format!(
                "node {node} already joined with a different node password; delete the node (kubectl delete node {node}) to let another machine take its name"
            ));
        }
        let _ = api.delete_named("", "secrets", "kube-system", &name);
    }
    let secret = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": name, "namespace": "kube-system", "labels": {"litek8s.dev/node-password": node}},
        "type": "litek8s.dev/node-password",
        "data": {"hash": hash},
    });
    match api.create_object("", "secrets", Some("kube-system"), secret) {
        Ok(_) => Ok(()),
        Err(e) if e.is_already_exists() => check_node_password(api, node, password),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_cidr(cidr: &str) -> Option<(u32, u8)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let addr: Ipv4Addr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((u32::from(addr) & mask, prefix))
}

/// The pod CIDR of `node`: the one its Node already has, else `requested`
/// when that is a free block of the cluster CIDR, else the lowest free block.
/// The Node object is created (spec only) if it does not exist yet.
pub(crate) fn allocate_pod_cidr(api: &ApiState, node: &str, requested: Option<&str>) -> Result<String, ApiError> {
    let _guard = api.join_lock().lock().unwrap();
    let net = api.cluster_network().clone();
    let (base, prefix) = parse_cidr(&net.cluster_cidr).ok_or_else(|| ApiError::bad_request("invalid cluster CIDR"))?;
    let mask = net.node_mask.max(prefix);
    let existing = api.get_object("", "nodes", "", node);
    if let Some(cidr) = existing.as_ref().map(|n| util::s(n, "/spec/podCIDR").to_string()).filter(|c| !c.is_empty()) {
        return Ok(cidr);
    }
    let (nodes, _) = api.store().list("", "nodes", None);
    let used: std::collections::HashSet<String> = nodes
        .iter()
        .filter(|n| util::name(n) != node)
        .map(|n| util::s(n, "/spec/podCIDR").to_string())
        .collect();
    let block = |i: u32| format!("{}/{mask}", Ipv4Addr::from(base + (i << (32 - mask))));
    let count = 1u32 << (mask - prefix);
    // A canonical block of this node size inside the cluster CIDR.
    let inside = |c: &str| {
        parse_cidr(c).is_some_and(|(b, p)| p == mask && (u64::from(b ^ base) >> (32 - u64::from(prefix))) == 0 && c == format!("{}/{mask}", Ipv4Addr::from(b)))
    };
    let cidr = requested
        .filter(|r| inside(r) && !used.contains(*r))
        .map(str::to_string)
        .or_else(|| (0..count).map(block).find(|c| !used.contains(c)))
        .ok_or_else(|| ApiError::new(hyper::StatusCode::CONFLICT, "Conflict", format!("no free /{mask} left in {}", net.cluster_cidr)))?;
    match existing {
        Some(mut obj) => {
            obj["spec"]["podCIDR"] = json!(cidr);
            obj["spec"]["podCIDRs"] = json!([cidr]);
            api.update_object("", "nodes", obj)?;
        }
        None => {
            let obj = json!({
                "apiVersion": "v1",
                "kind": "Node",
                "metadata": {"name": node, "labels": {"kubernetes.io/hostname": node}},
                "spec": {"podCIDR": cidr, "podCIDRs": [cidr]},
            });
            api.create_object("", "nodes", None, obj)?;
        }
    }
    Ok(cidr)
}

/// Label the API server's own node as the control plane (as k3s does), so
/// tools that keep workloads or disruption tests off it can tell.
pub(crate) fn mark_control_plane(api: &ApiState, node: &str) {
    // The node lifecycle controller may write the Node meanwhile (HA).
    for attempt in 0..5 {
        let Some(mut obj) = api.get_object("", "nodes", "", node) else { return };
        if util::labels_of(&obj).contains_key(CONTROL_PLANE_LABEL) {
            return;
        }
        if !obj["metadata"]["labels"].is_object() {
            obj["metadata"]["labels"] = json!({});
        }
        obj["metadata"]["labels"][CONTROL_PLANE_LABEL] = json!("true");
        match api.update_object("", "nodes", obj) {
            Ok(_) => return,
            Err(e) if e.is_conflict() && attempt < 4 => continue,
            Err(e) => return eprintln!("apiserver: could not label node {node} as control plane: {e}"),
        }
    }
}

pub const CONTROL_PLANE_LABEL: &str = "node-role.kubernetes.io/control-plane";

/// What a joining node learns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JoinResponse {
    pub pod_cidr: String,
    pub cluster_cidr: String,
    pub service_cidr: String,
    /// Every API server (`host:port`) of an HA control plane: the agent
    /// fails over between them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
}

/// Header of the agent connection's answer listing the API servers.
pub const SERVERS_HEADER: &str = "x-litek8s-servers";

/// The API servers agents may use (HA), as `host:port`.
pub(crate) fn server_list(api: &ApiState) -> Vec<String> {
    api.ha().map(|c| c.status().servers.into_iter().filter(|s| s.voter).map(|s| s.addr).collect()).unwrap_or_default()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinRequest {
    node: String,
    #[serde(default)]
    pod_cidr: Option<String>,
}

/// `POST /apis/agent.litek8s.dev/v1/join`.
pub(crate) fn handle_join(api: &ApiState, authorization: &str, password: &str, body: &[u8]) -> Result<Value, ApiError> {
    let request: JoinRequest = serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid join request: {e}")))?;
    authorize(api, authorization, &request.node, password)?;
    let pod_cidr = allocate_pod_cidr(api, &request.node, request.pod_cidr.as_deref())?;
    eprintln!("apiserver: node {} joined (pod CIDR {pod_cidr})", request.node);
    let response = JoinResponse {
        pod_cidr,
        cluster_cidr: api.cluster_network().cluster_cidr.clone(),
        service_cidr: SERVICE_CIDR.into(),
        servers: server_list(api),
    };
    Ok(serde_json::to_value(response).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Agent side

/// How an agent reaches and authenticates to the API server.
pub struct Client {
    https: bool,
    /// host:port of the server in use.
    authority: String,
    /// Every server known (HA): the one given, then those servers listed.
    servers: Vec<String>,
    token: Token,
    pub node: String,
    password: String,
    tls: Option<Arc<rustls::ClientConfig>>,
    dir: PathBuf,
}

/// A byte stream to the server, plain or TLS.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Verifies the server against the pinned CA. The certificate is checked
/// for one of the names it is always issued for, not the address dialed:
/// nodes join by IP and the serving certificate does not list every one.
#[derive(Debug)]
struct PinnedCa(Arc<rustls::client::WebPkiServerVerifier>);

impl rustls::client::danger::ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        ocsp: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let name = rustls::pki_types::ServerName::try_from("kubernetes.default.svc").expect("valid name");
        self.0.verify_server_cert(end_entity, intermediates, &name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Accepts any certificate: only for fetching the CA, which is then
/// checked against the token's hash.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn client_config(verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>) -> Result<Arc<rustls::ClientConfig>> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn pem_certificates(pem: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN CERTIFICATE") {
            current = Some(String::new());
        } else if line.starts_with("-----END CERTIFICATE") {
            if let Some(body) = current.take() {
                if let Ok(der) = b64.decode(body) {
                    out.push(rustls::pki_types::CertificateDer::from(der));
                }
            }
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
        }
    }
    out
}

impl Client {
    /// `dir` keeps the node password, the server CA and the join result.
    pub fn new(server: &str, token: &str, node: &str, dir: &Path) -> Result<Self> {
        let server = server.trim_end_matches('/');
        let (https, authority) = if let Some(rest) = server.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = server.strip_prefix("http://") {
            (false, rest)
        } else {
            (true, server)
        };
        let authority = if authority.contains(':') { authority.to_string() } else { format!("{authority}:6443") };
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let mut servers = vec![authority.clone()];
        let saved: Vec<String> = std::fs::read(dir.join("servers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for s in saved {
            if !servers.contains(&s) {
                servers.push(s);
            }
        }
        Ok(Self {
            https,
            authority,
            servers,
            token: Token::parse(token)?,
            node: node.to_string(),
            password: load_or_create(&dir.join("node-password"), 24)?,
            tls: None,
            dir: dir.to_path_buf(),
        })
    }

    /// Learn the cluster's servers (kept for restarts).
    pub fn set_servers(&mut self, list: &[String]) {
        let mut changed = false;
        for s in list {
            if !s.is_empty() && !self.servers.contains(s) {
                self.servers.push(s.clone());
                changed = true;
            }
        }
        if changed {
            let _ = std::fs::write(self.dir.join("servers.json"), serde_json::to_vec(&self.servers[1..]).unwrap_or_default());
        }
    }

    /// Move on to the next known server.
    pub fn next_server(&mut self) {
        if self.servers.len() > 1 {
            let i = self.servers.iter().position(|s| *s == self.authority).map_or(0, |i| (i + 1) % self.servers.len());
            self.authority = self.servers[i].clone();
            eprintln!("node-agent: trying API server {}", self.authority);
        }
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Whether the server in use answers `/readyz` (an HA server cut off
    /// from the others does not).
    pub async fn ready(&mut self) -> bool {
        let probe = async {
            self.ensure_tls().await?;
            let io = self.dial(self.tls.as_ref()).await?;
            let (status, _) = request(io, &self.authority, "GET", "/readyz", &[], Vec::new()).await?;
            Ok::<bool, anyhow::Error>(status == 200)
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), probe).await.ok().and_then(|r| r.ok()).unwrap_or(false)
    }

    pub fn server_host(&self) -> &str {
        self.authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(&self.authority).trim_matches(['[', ']'])
    }

    async fn dial(&self, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<Box<dyn Io>> {
        let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::TcpStream::connect(&self.authority))
            .await
            .with_context(|| format!("timed out connecting to {}", self.authority))??;
        let _ = tcp.set_nodelay(true);
        // Notice a peer that vanished (a sleeping laptop, a dead VM) on an
        // idle connection; the protocol's heartbeats cover the busy case.
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(20))
            .with_interval(std::time::Duration::from_secs(5));
        let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
        if !self.https {
            return Ok(Box::new(tcp));
        }
        let config = tls.context("no TLS configuration")?;
        let name = rustls::pki_types::ServerName::try_from("kubernetes.default.svc")?;
        let stream = tokio_rustls::TlsConnector::from(config.clone())
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS handshake with {} failed", self.authority))?;
        Ok(Box::new(stream))
    }

    /// Trust the server: the CA saved on an earlier join, or the one it
    /// serves now, checked against the token's hash.
    async fn ensure_tls(&mut self) -> Result<()> {
        if !self.https || self.tls.is_some() {
            return Ok(());
        }
        let path = self.dir.join("server-ca.pem");
        let saved = std::fs::read_to_string(&path).ok().filter(|pem| self.token.ca_hash.as_ref().is_none_or(|h| *h == sha256_hex(pem)));
        let pem = match saved {
            Some(pem) => pem,
            None => {
                let any = client_config(Arc::new(AnyCert(provider())))?;
                let io = self.dial(Some(&any)).await?;
                let (status, body) = request(io, &self.authority, "GET", "/cacerts", &[], Vec::new()).await?;
                if status != 200 {
                    bail!("GET /cacerts answered {status}");
                }
                let pem = String::from_utf8(body).context("CA is not text")?;
                match &self.token.ca_hash {
                    Some(hash) if *hash != sha256_hex(&pem) => bail!("the server's CA does not match the join token (wrong server, or a man in the middle)"),
                    Some(_) => {}
                    None => eprintln!("node-agent: the join token pins no CA; trusting the server's CA on first use"),
                }
                std::fs::write(&path, &pem).with_context(|| format!("failed to write {}", path.display()))?;
                pem
            }
        };
        self.tls = Some(pinned_client_config(&pem)?);
        Ok(())
    }

    fn credentials(&self) -> Vec<(&'static str, String)> {
        vec![("authorization", format!("Bearer {}", self.token.secret)), (PASSWORD_HEADER, self.password.clone())]
    }

    /// Register with the server and learn the pod CIDR. Falls back to the
    /// result of an earlier join when the server is unreachable, so a node
    /// restarts without its control plane.
    pub async fn join(&mut self) -> Result<JoinResponse> {
        let saved_path = self.dir.join("join.json");
        let saved: Option<JoinResponse> = std::fs::read(&saved_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let attempt = async {
            self.ensure_tls().await?;
            let io = self.dial(self.tls.as_ref()).await?;
            let body = json!({"node": self.node, "podCIDR": saved.as_ref().map(|s| s.pod_cidr.clone())});
            let headers = self.credentials();
            let (status, body) = request(io, &self.authority, "POST", "/apis/agent.litek8s.dev/v1/join", &headers, serde_json::to_vec(&body)?).await?;
            if status != 200 {
                let message = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string));
                bail!("join refused ({status}): {}", message.unwrap_or_else(|| String::from_utf8_lossy(&body).to_string()));
            }
            Ok::<JoinResponse, anyhow::Error>(serde_json::from_slice(&body).context("invalid join response")?)
        };
        match attempt.await {
            Ok(response) => {
                std::fs::write(&saved_path, serde_json::to_vec(&response)?)?;
                self.set_servers(&response.servers);
                Ok(response)
            }
            Err(e) => match saved {
                // A refusal is final; an unreachable server is not.
                Some(saved) if !format!("{e:#}").contains("refused (") => {
                    eprintln!("node-agent: join failed ({e:#}); using the pod CIDR from the last join");
                    Ok(saved)
                }
                _ => Err(e),
            },
        }
    }

    /// Open the agent connection: one HTTP request upgraded to the protocol.
    pub async fn connect(&mut self) -> Result<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>> {
        use hyper::body::Bytes;
        self.ensure_tls().await?;
        let io = self.dial(self.tls.as_ref()).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let mut req = hyper::Request::builder()
            .uri(format!("/apis/agent.litek8s.dev/v1/connect?node={}", self.node))
            .header(hyper::header::HOST, &self.authority)
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(hyper::header::UPGRADE, super::agent::protocol::PROTOCOL_VERSION);
        for (k, v) in self.credentials() {
            req = req.header(k, v);
        }
        let resp = sender.send_request(req.body(http_body_util::Empty::<Bytes>::new())?).await?;
        if resp.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
            use http_body_util::BodyExt;
            let status = resp.status();
            let body = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            bail!("server answered {status}: {}", String::from_utf8_lossy(&body).trim());
        }
        let servers: Vec<String> = resp
            .headers()
            .get(SERVERS_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        self.set_servers(&servers);
        let upgraded = hyper::upgrade::on(resp).await?;
        Ok(hyper_util::rt::TokioIo::new(upgraded))
    }
}

/// A TLS client configuration that trusts only the cluster CA `ca_pem`
/// (checked for the name every serving certificate carries).
pub(crate) fn pinned_client_config(ca_pem: &str) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certificates(ca_pem) {
        roots.add(cert)?;
    }
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build()?;
    client_config(Arc::new(PinnedCa(verifier)))
}

/// The name TLS clients check server certificates for.
pub(crate) const SERVER_NAME: &str = "kubernetes.default.svc";

/// Fetch the cluster CA from `authority` (`host:port`) and check it against
/// the token's hash.
pub(crate) async fn fetch_ca(authority: &str, token: &Token) -> Result<String> {
    let any = client_config(Arc::new(AnyCert(provider())))?;
    let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::TcpStream::connect(authority))
        .await
        .with_context(|| format!("timed out connecting to {authority}"))??;
    let name = rustls::pki_types::ServerName::try_from(SERVER_NAME)?;
    let io = tokio_rustls::TlsConnector::from(any).connect(name, tcp).await?;
    let (status, body) = request(Box::new(io), authority, "GET", "/cacerts", &[], Vec::new()).await?;
    if status != 200 {
        bail!("GET /cacerts answered {status}");
    }
    let pem = String::from_utf8(body).context("CA is not text")?;
    match &token.ca_hash {
        Some(hash) if *hash != sha256_hex(&pem) => bail!("the server's CA does not match the join token (wrong server, or a man in the middle)"),
        _ => Ok(pem),
    }
}

/// The status of `GET path` over TLS without checking the certificate: only
/// for health checks (`litek8s lb --https-check`), which trust nothing in the
/// answer but its status.
pub(crate) async fn unverified_status(authority: &str, path: &str, timeout: std::time::Duration) -> Result<u16> {
    let any = client_config(Arc::new(AnyCert(provider())))?;
    tokio::time::timeout(timeout, async {
        let tcp = tokio::net::TcpStream::connect(authority).await?;
        let name = rustls::pki_types::ServerName::try_from(SERVER_NAME)?;
        let io = tokio_rustls::TlsConnector::from(any).connect(name, tcp).await?;
        Ok(request(Box::new(io), authority, "GET", path, &[], Vec::new()).await?.0)
    })
    .await
    .with_context(|| format!("timed out checking {authority}"))?
}

/// One HTTP/1.1 request on a fresh connection.
async fn request(io: Box<dyn Io>, authority: &str, method: &str, path: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<(u16, Vec<u8>)> {
    use http_body_util::BodyExt;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, authority)
        .header(hyper::header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = sender.send_request(req.body(http_body_util::Full::new(hyper::body::Bytes::from(body)))?).await?;
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await?.to_bytes().to_vec();
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_roundtrip() {
        let token = Token::format(Some("pem"), "s3cret");
        assert_eq!(token, format!("K10{}::s3cret", sha256_hex("pem")));
        let parsed = Token::parse(&token).unwrap();
        assert_eq!(parsed, Token { ca_hash: Some(sha256_hex("pem")), secret: "s3cret".into() });
        assert_eq!(Token::parse("plain").unwrap(), Token { ca_hash: None, secret: "plain".into() });
        assert_eq!(Token::parse("K10::server:pw").unwrap().secret, "pw");
        assert!(Token::parse("K10abc::x").is_err());
        assert!(Token::parse("").is_err());
    }

    #[test]
    fn allocates_distinct_node_cidrs_and_keeps_them() {
        let api = ApiState::open(None, "127.0.0.1:0".into(), false).unwrap();
        let a = allocate_pod_cidr(&api, "a", None).unwrap();
        let b = allocate_pod_cidr(&api, "b", Some("10.42.7.0/24")).unwrap();
        let c = allocate_pod_cidr(&api, "c", Some("10.42.7.0/24")).unwrap();
        let outside = allocate_pod_cidr(&api, "d", Some("192.168.0.0/24")).unwrap();
        assert_eq!((a.as_str(), b.as_str(), c.as_str(), outside.as_str()), ("10.42.0.0/24", "10.42.7.0/24", "10.42.1.0/24", "10.42.2.0/24"));
        assert_eq!(allocate_pod_cidr(&api, "a", Some("10.42.9.0/24")).unwrap(), a);
    }

    #[test]
    fn node_passwords_protect_names() {
        let api = ApiState::open(None, "127.0.0.1:0".into(), false).unwrap();
        let auth = format!("Bearer {}", api.join_secret());
        assert!(authorize(&api, "Bearer wrong", "n1", "0123456789abcdef0123").is_err());
        authorize(&api, &auth, "n1", "0123456789abcdef0123").unwrap();
        allocate_pod_cidr(&api, "n1", None).unwrap();
        authorize(&api, &auth, "n1", "0123456789abcdef0123").unwrap();
        let taken = authorize(&api, &auth, "n1", "another-password-0000").unwrap_err();
        assert_eq!(taken.code, hyper::StatusCode::FORBIDDEN);
        // Once the node is deleted, its name is free again.
        api.delete_named("", "nodes", "", "n1").unwrap();
        authorize(&api, &auth, "n1", "another-password-0000").unwrap();
        assert!(authorize(&api, &auth, "Bad_Name", "0123456789abcdef0123").is_err());
    }

    #[test]
    fn reads_pem_certificates() {
        let pem = "-----BEGIN CERTIFICATE-----\nAAEC\nAw==\n-----END CERTIFICATE-----\n";
        let certs = pem_certificates(pem);
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].as_ref(), &[0, 1, 2, 3]);
    }
}
