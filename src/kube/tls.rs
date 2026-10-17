//! TLS for in-cluster clients.
//!
//! Pods reach the API server the way client-go's in-cluster config expects:
//! `https://kubernetes.default.svc` with the cluster CA from their service
//! account volume. The server makes its own CA and serving certificate on
//! first start (kept in the data directory when there is one) and serves TLS
//! on the same port as plain HTTP, telling them apart by the first byte.
//! There is no client authentication yet: any bearer token is accepted.
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
use tokio_rustls::TlsAcceptor;

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
    acceptor: TlsAcceptor,
    ca_pem: String,
}

impl ServerTls {
    pub fn load_or_create(data_dir: Option<&Path>) -> Result<Self> {
        if let Some(dir) = data_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let path = data_dir.map(|d| d.join("tls.json"));
        let stored = match path.as_ref().filter(|p| p.exists()) {
            Some(p) => serde_json::from_slice(&std::fs::read(p)?).with_context(|| format!("failed to read {}", p.display()))?,
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
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert, ca], key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            ca_pem: stored.ca_pem,
        })
    }

    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }

    /// The cluster CA, as published in every namespace's `kube-root-ca.crt`.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }
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
