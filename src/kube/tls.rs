//! TLS for in-cluster clients.
//!
//! Pods reach the API server the way client-go's in-cluster config expects:
//! `https://kubernetes.default.svc` with the cluster CA from their service
//! account volume. The server makes its own CA and serving certificate on
//! first start (kept in the data directory when there is one) and serves TLS
//! on the same port as plain HTTP, telling them apart by the first byte.
//! There is no client authentication yet: any bearer token is accepted.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
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

fn hostname() -> Result<String> {
    let out = std::process::Command::new("hostname").output()?;
    let name = String::from_utf8(out.stdout)?.trim().to_lowercase();
    anyhow::ensure!(!name.is_empty(), "empty hostname");
    Ok(name)
}
