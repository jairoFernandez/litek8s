//! TLS for in-cluster clients.
//!
//! Pods reach the API server the way client-go's in-cluster config expects:
//! `https://kubernetes.default.svc` with the cluster CA from their service
//! account volume. The server makes its own CA and serving certificate on
//! first start (kept in the data directory when there is one) and serves TLS
//! on the same port as plain HTTP, telling them apart by the first byte.
//! Clients may present a certificate signed by the cluster's client CA
//! ([`ClientCa`], derived from the service account key so that every HA
//! server has the same one); its CN is the user and its O entries the
//! groups. Presenting none is fine at the TLS level (bearer tokens,
//! anonymous requests); `authn` decides what the request may do.
//!
//! rustls sends its whole first flight (ServerHello, Certificate,
//! ServerKeyExchange, ServerHelloDone) in a single TLS record. That is valid,
//! but busybox's built-in TLS client (`wget` in busybox images, common in
//! init containers and probes) expects one handshake message per record and
//! stalls. [`OneMessagePerRecord`] re-frames the plaintext handshake records
//! the server writes, which every client accepts; versions, cipher suites and
//! keys are rustls's defaults.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, ready};

use anyhow::{Context, Result};
use base64::Engine;
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Names the serving certificate is valid for.
const SERVER_NAMES: &[&str] = &[
    "localhost",
    "kubernetes",
    "kubernetes.default",
    "kubernetes.default.svc",
    "kubernetes.default.svc.cluster.local",
    "host.docker.internal",
    "127.0.0.1",
    "::1",
    "10.43.0.1",
];

#[derive(Serialize, Deserialize)]
struct Stored {
    ca_pem: String,
    ca_der: String,
    cert_der: String,
    key_der: String,
}

pub struct ServerTls {
    config: Arc<rustls::ServerConfig>,
    /// Signs client certificates (the admin kubeconfig's).
    client_ca: ClientCa,
    /// The same, with X25519 as the only key exchange group (see `accept`).
    x25519: Arc<rustls::ServerConfig>,
    /// For Raft connections between servers (ALPN [`RAFT_ALPN`]).
    raft: Arc<rustls::ServerConfig>,
    ca_pem: String,
}

/// The ALPN protocol of HA servers' Raft connections (their own listener).
pub const RAFT_ALPN: &[u8] = b"litek8s-raft";

impl ServerTls {
    /// The serving certificate from `<data_dir>/tls.json` (created if
    /// missing) and the client CA derived from `client_ca_seed`.
    pub fn load_or_create(data_dir: Option<&Path>, client_ca_seed: [u8; 32]) -> Result<Self> {
        if let Some(dir) = data_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let path = data_dir.map(|d| d.join("tls.json"));
        let existing: Option<Stored> = match path.as_ref().filter(|p| p.exists()) {
            Some(p) => Some(serde_json::from_slice(&std::fs::read(p)?).with_context(|| format!("failed to read {}", p.display()))?),
            None => None,
        };
        // Certificates from before the authority key identifier was added are
        // rejected by strict verifiers (Python 3.13's ssl): make new ones.
        let existing = existing.filter(|s| {
            let b64 = base64::engine::general_purpose::STANDARD;
            b64.decode(&s.cert_der).is_ok_and(|der| has_authority_key_identifier(&der))
        });
        let stored = match existing {
            Some(s) => s,
            None => {
                let fresh = generate()?;
                if let Some(p) = &path {
                    std::fs::write(p, serde_json::to_vec(&fresh)?).with_context(|| format!("failed to write {}", p.display()))?;
                }
                fresh
            }
        };
        let b64 = base64::engine::general_purpose::STANDARD;
        let cert = CertificateDer::from(b64.decode(&stored.cert_der)?);
        let ca = CertificateDer::from(b64.decode(&stored.ca_der)?);
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(b64.decode(&stored.key_der)?));
        let client_ca = ClientCa::derive(client_ca_seed)?;
        if let Some(dir) = data_dir {
            client_ca.write_files(dir)?;
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(client_ca.cert_der.clone()))?;
        let roots = Arc::new(roots);
        let build = |provider: rustls::crypto::CryptoProvider| -> Result<Arc<rustls::ServerConfig>> {
            let provider = Arc::new(provider);
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots.clone(), provider.clone())
                .allow_unauthenticated()
                .build()?;
            let mut config = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_client_cert_verifier(verifier)
                .with_single_cert(vec![cert.clone(), ca.clone()], key.clone_key())?;
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(Arc::new(config))
        };
        let mut x25519 = rustls::crypto::ring::default_provider();
        x25519.kx_groups = vec![rustls::crypto::ring::kx_group::X25519];
        let config = build(rustls::crypto::ring::default_provider())?;
        let mut raft = (*config).clone();
        raft.alpn_protocols = vec![RAFT_ALPN.to_vec()];
        Ok(Self {
            config,
            client_ca,
            x25519: build(x25519)?,
            raft: Arc::new(raft),
            ca_pem: stored.ca_pem,
        })
    }

    /// Accept a TLS connection. rustls takes the key exchange group the
    /// client lists first; busybox's client lists P-256 first, and its
    /// x86_64 P-256 code sends invalid key shares (rustls: InvalidKeyShare,
    /// the client sees alert 47). So a client that offers X25519 gets X25519,
    /// as many servers prefer anyway; others get the default groups.
    pub async fn accept<IO>(&self, io: IO) -> std::io::Result<tokio_rustls::server::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let start = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), io).await?;
        let offers_x25519 = start
            .client_hello()
            .named_groups()
            .is_some_and(|groups| groups.contains(&rustls::NamedGroup::X25519));
        let config = if offers_x25519 { self.x25519.clone() } else { self.config.clone() };
        start.into_stream(config).await
    }

    /// The cluster CA, as published in every namespace's `kube-root-ca.crt`.
    /// Accept a Raft connection (ALPN [`RAFT_ALPN`]).
    pub async fn accept_raft<IO>(&self, io: IO) -> std::io::Result<tokio_rustls::server::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        tokio_rustls::TlsAcceptor::from(self.raft.clone()).accept(io).await
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn client_ca(&self) -> &ClientCa {
        &self.client_ca
    }

    /// A kubeconfig for `server` as `system:admin` (group `system:masters`)
    /// with a client certificate. `tls_server_name` when `server`'s host is
    /// not one the serving certificate names.
    pub fn admin_kubeconfig(&self, server: &str, tls_server_name: Option<&str>) -> Result<String> {
        let (cert, key) = self.client_ca.issue(ADMIN_USER, Some(crate::kube::rbac::MASTERS))?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let name_line = tls_server_name.map(|n| format!("    tls-server-name: {n}\n")).unwrap_or_default();
        Ok(format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: litek8s\n  cluster:\n    server: {server}\n    certificate-authority-data: {}\n{name_line}\
             contexts:\n- name: litek8s\n  context:\n    cluster: litek8s\n    user: litek8s\n\
             current-context: litek8s\nusers:\n- name: litek8s\n  user:\n    client-certificate-data: {}\n    client-key-data: {}\n",
            b64.encode(&self.ca_pem),
            b64.encode(cert),
            b64.encode(key),
        ))
    }
}

/// The user of the admin kubeconfig.
pub const ADMIN_USER: &str = "system:admin";

/// The CA that signs client certificates (kube-apiserver's
/// `--client-ca-file`). Its Ed25519 key comes from a seed (a hash of the
/// service account key, which HA servers share), so it is the same on
/// every server without being stored; the certificate is deterministic
/// too. Written to `<data dir>/client-ca.{crt,key}` for operators who sign
/// their own users' certificates.
pub struct ClientCa {
    issuer: rcgen::Certificate,
    key: KeyPair,
    cert_der: Vec<u8>,
}

impl ClientCa {
    pub fn derive(seed: [u8; 32]) -> Result<Self> {
        // PKCS#8 v1 of an Ed25519 key is a fixed prefix and the seed.
        let mut pkcs8 = hex_bytes("302e020100300506032b657004220420");
        pkcs8.extend_from_slice(&seed);
        let key = KeyPair::from_pkcs8_der_and_sign_algo(&PrivatePkcs8KeyDer::from(pkcs8), &rcgen::PKCS_ED25519)?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, "litek8s-client-ca");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(2125, 1, 1);
        params.serial_number = Some(rcgen::SerialNumber::from(1u64));
        let issuer = params.self_signed(&key)?;
        let cert_der = issuer.der().to_vec();
        Ok(Self { issuer, key, cert_der })
    }

    pub fn cert_pem(&self) -> String {
        self.issuer.pem()
    }

    /// A client certificate and its key (PEM) for `user` in `group`, valid
    /// for ten years. (rcgen writes one entry per attribute type, so one
    /// group; certificates made elsewhere may name several.)
    pub fn issue(&self, user: &str, group: Option<&str>) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(DnType::CommonName, user);
        if let Some(g) = group {
            params.distinguished_name.push(DnType::OrganizationName, g);
        }
        params.use_authority_key_identifier_extension = true;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        use chrono::Datelike;
        let ymd = |d: chrono::DateTime<chrono::Utc>| rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8);
        let now = chrono::Utc::now();
        params.not_before = ymd(now - chrono::Duration::days(1));
        params.not_after = ymd(now + chrono::Duration::days(3650));
        let cert = params.signed_by(&key, &self.issuer, &self.key)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// `client-ca.crt` and `client-ca.key` (0600) in `dir`.
    fn write_files(&self, dir: &Path) -> Result<()> {
        let crt = dir.join("client-ca.crt");
        let key = dir.join("client-ca.key");
        let pem = self.cert_pem();
        if std::fs::read_to_string(&crt).ok().as_deref() != Some(pem.as_str()) {
            std::fs::write(&crt, &pem).with_context(|| format!("failed to write {}", crt.display()))?;
        }
        let key_pem = self.key.serialize_pem();
        if std::fs::read_to_string(&key).ok().as_deref() != Some(key_pem.as_str()) {
            std::fs::write(&key, &key_pem).with_context(|| format!("failed to write {}", key.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or_default()).collect()
}

/// The user (CN) and groups (O) a client certificate names: DER walked by
/// hand (TBSCertificate → subject Name), the chain was verified by rustls.
pub fn certificate_subject(der: &[u8]) -> Option<(String, Vec<String>)> {
    // (tag, content, rest)
    fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *b.first()?;
        let first = *b.get(1)? as usize;
        let (len, head) = if first < 0x80 {
            (first, 2)
        } else {
            let n = first & 0x7f;
            if n == 0 || n > 4 {
                return None;
            }
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | *b.get(2 + i)? as usize;
            }
            (len, 2 + n)
        };
        let end = head.checked_add(len)?;
        Some((tag, b.get(head..end)?, b.get(end..)?))
    }
    let (_, cert, _) = tlv(der)?;
    let (_, mut tbs, _) = tlv(cert)?;
    // [0] version (optional), serial, signature algorithm, issuer, validity, subject
    if tbs.first() == Some(&0xa0) {
        tbs = tlv(tbs)?.2;
    }
    for _ in 0..4 {
        tbs = tlv(tbs)?.2;
    }
    let (_, mut name, _) = tlv(tbs)?;
    const CN: &[u8] = &[0x55, 0x04, 0x03];
    const O: &[u8] = &[0x55, 0x04, 0x0a];
    let (mut user, mut groups) = (None, Vec::new());
    while !name.is_empty() {
        let (_, mut rdn, rest) = tlv(name)?;
        name = rest;
        while !rdn.is_empty() {
            let (_, attr, rest) = tlv(rdn)?;
            rdn = rest;
            let (_, oid, value) = tlv(attr)?;
            let (_, value, _) = tlv(value)?;
            let value = String::from_utf8_lossy(value).to_string();
            if oid == CN {
                user = Some(value);
            } else if oid == O {
                groups.push(value);
            }
        }
    }
    Some((user?, groups))
}

fn generate() -> Result<Stored> {
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "litek8s-ca");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    ca_params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    ca_params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let ca = ca_params.self_signed(&ca_key)?;

    let mut names: Vec<String> = SERVER_NAMES.iter().map(|s| s.to_string()).collect();
    if let Ok(host) = hostname() {
        names.push(host);
    }
    let server_key = KeyPair::generate()?;
    let mut params = CertificateParams::new(names)?;
    // Strict X.509 verification (Python 3.13's default ssl context, used by
    // e.g. the kubernetes Python client) requires it on non-root certificates.
    params.use_authority_key_identifier_extension = true;
    params.distinguished_name.push(DnType::CommonName, "kube-apiserver");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let cert = params.signed_by(&server_key, &ca, &ca_key)?;

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(Stored {
        ca_pem: ca.pem(),
        ca_der: b64.encode(ca.der()),
        cert_der: b64.encode(cert.der()),
        key_der: b64.encode(server_key.serialize_der()),
    })
}

/// Whether a DER certificate carries an authority key identifier extension
/// (OID 2.5.29.35).
fn has_authority_key_identifier(der: &[u8]) -> bool {
    der.windows(5).any(|w| w == [0x06, 0x03, 0x55, 0x1d, 0x23])
}

const CONTENT_HANDSHAKE: u8 = 22;
const RECORD_HEADER: usize = 5;

/// A server-side transport that splits each plaintext TLS handshake record
/// into one record per handshake message, until the server's first
/// ChangeCipherSpec, alert or application data record; after that bytes pass
/// through untouched. Reads are not affected.
pub struct OneMessagePerRecord<S> {
    inner: S,
    /// Bytes of an incomplete record written so far (only while splitting).
    partial: Vec<u8>,
    /// Re-framed bytes not yet written to `inner`.
    out: Vec<u8>,
    out_pos: usize,
    splitting: bool,
}

impl<S> OneMessagePerRecord<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, partial: Vec::new(), out: Vec::new(), out_pos: 0, splitting: true }
    }

    /// Move every complete record in `partial` to `out`, re-framed.
    fn reframe(&mut self) {
        let mut pos = 0;
        while self.splitting && self.partial.len() - pos >= RECORD_HEADER {
            let header = &self.partial[pos..pos + RECORD_HEADER];
            let len = u16::from_be_bytes([header[3], header[4]]) as usize;
            if self.partial.len() - pos < RECORD_HEADER + len {
                break;
            }
            let record = &self.partial[pos..pos + RECORD_HEADER + len];
            if record[0] == CONTENT_HANDSHAKE {
                split_handshake_record(record, &mut self.out);
            } else {
                self.out.extend_from_slice(record);
                self.splitting = false;
            }
            pos += RECORD_HEADER + len;
        }
        if !self.splitting {
            // Whatever follows the last record we looked at is passed on as is.
            self.out.extend_from_slice(&self.partial[pos..]);
            pos = self.partial.len();
        }
        self.partial.drain(..pos);
    }
}

/// Append `record` to `out` as one record per handshake message. A record
/// that does not hold whole messages (a message split across records) is
/// appended unchanged.
fn split_handshake_record(record: &[u8], out: &mut Vec<u8>) {
    let body = &record[RECORD_HEADER..];
    let mut messages = Vec::new();
    let mut pos = 0;
    while body.len() - pos >= 4 {
        let len = 4 + u32::from_be_bytes([0, body[pos + 1], body[pos + 2], body[pos + 3]]) as usize;
        if body.len() - pos < len {
            break;
        }
        messages.push(&body[pos..pos + len]);
        pos += len;
    }
    if pos != body.len() || messages.len() < 2 {
        out.extend_from_slice(record);
        return;
    }
    for message in messages {
        out.extend_from_slice(&record[..3]);
        out.extend_from_slice(&(message.len() as u16).to_be_bytes());
        out.extend_from_slice(message);
    }
}

impl<S: AsyncWrite + Unpin> OneMessagePerRecord<S> {
    fn poll_drain(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        while self.out_pos < self.out.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.out_pos += n;
        }
        self.out.clear();
        self.out_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for OneMessagePerRecord<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for OneMessagePerRecord<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        ready!(this.poll_drain(cx))?;
        if !this.splitting {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.partial.extend_from_slice(buf);
        this.reframe();
        // The bytes are ours now; start sending them, finish in a later call.
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn hostname() -> Result<String> {
    let out = std::process::Command::new("hostname").output()?;
    let name = String::from_utf8(out.stdout)?.trim().to_lowercase();
    anyhow::ensure!(!name.is_empty(), "empty hostname");
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut r = vec![kind, 3, 3];
        r.extend_from_slice(&(body.len() as u16).to_be_bytes());
        r.extend_from_slice(body);
        r
    }

    fn message(kind: u8, len: usize) -> Vec<u8> {
        let mut m = vec![kind, 0, (len >> 8) as u8, len as u8];
        m.extend(std::iter::repeat_n(kind, len));
        m
    }

    #[test]
    fn client_ca_is_deterministic_and_issues_certificates() {
        let a = ClientCa::derive([7; 32]).unwrap();
        let b = ClientCa::derive([7; 32]).unwrap();
        assert_eq!(a.cert_der, b.cert_der);
        assert_ne!(a.cert_der, ClientCa::derive([8; 32]).unwrap().cert_der);
        let (cert, _key) = a.issue("jane", Some("system:masters")).unwrap();
        use rustls::pki_types::pem::PemObject;
        let der = CertificateDer::from_pem_slice(cert.as_bytes()).unwrap();
        assert_eq!(certificate_subject(&der), Some(("jane".into(), vec!["system:masters".into()])));
        // Accepted by the verifier the server uses.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(b.cert_der.clone())).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        verifier.verify_client_cert(&der, &[], rustls::pki_types::UnixTime::now()).unwrap();
        let other = ClientCa::derive([9; 32]).unwrap().issue("eve", Some("system:masters")).unwrap().0;
        let other = CertificateDer::from_pem_slice(other.as_bytes()).unwrap();
        assert!(verifier.verify_client_cert(&other, &[], rustls::pki_types::UnixTime::now()).is_err());
    }

    #[test]
    fn subject_with_several_organizations() {
        fn tlv(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
            let body = parts.concat();
            let mut out = vec![tag];
            if body.len() < 128 {
                out.push(body.len() as u8);
            } else {
                out.extend([0x81, body.len() as u8]);
            }
            out.extend(body);
            out
        }
        let attr = |oid: &[u8], v: &str| tlv(0x31, &[&tlv(0x30, &[&tlv(0x06, &[oid]), &tlv(0x0c, &[v.as_bytes()])])]);
        let subject = tlv(0x30, &[&attr(&[0x55, 4, 3], "jane"), &attr(&[0x55, 4, 10], "devs"), &attr(&[0x55, 4, 10], "ops")]);
        let empty = tlv(0x30, &[]);
        let tbs = tlv(0x30, &[&tlv(0xa0, &[&tlv(0x02, &[&[2]])]), &tlv(0x02, &[&[1]]), &empty, &empty, &empty, &subject]);
        let cert = tlv(0x30, &[&tbs, &empty, &tlv(0x03, &[&[0]])]);
        assert_eq!(certificate_subject(&cert), Some(("jane".into(), vec!["devs".into(), "ops".into()])));
        assert_eq!(certificate_subject(&cert[..cert.len() - 4]), None);
    }

    #[test]
    fn serving_certificate_names_its_issuer_key() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let stored = generate().unwrap();
        assert!(has_authority_key_identifier(&b64.decode(&stored.cert_der).unwrap()));
    }

    #[tokio::test]
    async fn splits_the_first_flight_and_passes_the_rest() {
        use tokio::io::AsyncWriteExt;
        let (hello, cert, done) = (message(2, 40), message(11, 300), message(14, 0));
        let flight = record(22, &[hello.clone(), cert.clone(), done.clone()].concat());
        let ccs = record(20, &[1]);
        // After ChangeCipherSpec, records are encrypted and must not be touched.
        let encrypted = record(22, &[hello.clone(), done.clone()].concat());
        let mut wrapped = OneMessagePerRecord::new(Vec::new());
        // Written in odd-sized pieces, as a TLS stack may.
        let all = [flight, ccs.clone(), encrypted.clone()].concat();
        for chunk in all.chunks(7) {
            wrapped.write_all(chunk).await.unwrap();
        }
        wrapped.flush().await.unwrap();
        let expected = [record(22, &hello), record(22, &cert), record(22, &done), ccs, encrypted].concat();
        assert_eq!(wrapped.inner, expected);
    }
}
