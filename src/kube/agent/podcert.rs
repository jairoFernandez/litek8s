//! `podCertificate` projected volume sources, as the kubelet does them: a
//! fresh key pair per certificate, a PodCertificateRequest (a stub PKCS#10
//! request, created by the API server for this node's pod, see
//! `hub::create_pcr`), wait for an external signer to issue it, then keep
//! the key and chain; a new key and request from `beginRefreshAt` on, while
//! the current certificate keeps being written. Plus the PEM normalization
//! the kubelet applies to chains and trust bundles.
//!
//! The state of each projection lives in the pod's directory
//! (`podcerts/<volume>-<source>.json`, 0600), so an agent restart neither
//! loses the key nor asks for a new certificate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::kube::{certificates, x509};

/// How often a pending request is looked at again (answers also wake the
/// pod, see `hub.rs`).
pub(crate) const POLL_SECS: i64 = 5;
/// Wait before a new request after one was denied or failed.
const RETRY_SECS: i64 = 30;

/// What the API server is asked through: the agent's connection, or a
/// signer in tests.
pub(crate) trait PcrApi {
    /// Create a request for the pod (`hub::create_pcr`'s JSON); the
    /// created PodCertificateRequest.
    async fn create(&self, ns: &str, request: Value) -> Result<Value, String>;
    async fn get(&self, ns: &str, name: &str) -> Result<Option<Value>, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Issued {
    /// PKCS#8 PEM.
    pub key_pem: String,
    /// Normalized: CERTIFICATE blocks only, leaf first.
    pub chain_pem: String,
    pub not_after: i64,
    pub begin_refresh_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Pending {
    name: String,
    key_pem: String,
    /// Last looked at.
    #[serde(default)]
    checked_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct State {
    #[serde(default)]
    current: Option<Issued>,
    #[serde(default)]
    pending: Option<Pending>,
    #[serde(default)]
    error: Option<String>,
    /// No new request before this (after a denial).
    #[serde(default)]
    retry_at: i64,
}

pub(crate) fn state_dir(pod_dir: &Path) -> PathBuf {
    pod_dir.join("podcerts")
}

fn load(path: &Path) -> State {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(path: &Path, st: &State) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
    f.write_all(&serde_json::to_vec(st).unwrap_or_default()).map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// A new key pair for `key_type`: (PKCS#8 PEM, DER stub PKCS#10 request).
pub(crate) fn new_key(key_type: &str) -> Result<(String, Vec<u8>), String> {
    let alg = match key_type {
        "ECDSAP256" => &rcgen::PKCS_ECDSA_P256_SHA256,
        "ECDSAP384" => &rcgen::PKCS_ECDSA_P384_SHA384,
        "ED25519" => &rcgen::PKCS_ED25519,
        other => return Err(format!("keyType {other} is not supported on litek8s nodes yet (use ECDSAP256, ECDSAP384 or ED25519)")),
    };
    let key = rcgen::KeyPair::generate_for(alg).map_err(|e| format!("failed to generate a {key_type} key: {e}"))?;
    // The kubelet's stub request is empty: no subject, no attributes.
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key).map_err(|e| format!("failed to make a certificate request: {e}"))?;
    Ok((key.serialize_pem(), csr.der().to_vec()))
}

/// CERTIFICATE blocks only, without headers or data in between, in order.
pub(crate) fn normalize_chain(pem: &str) -> String {
    x509::pem_blocks(pem)
        .unwrap_or_default()
        .iter()
        .filter(|b| b.label == "CERTIFICATE")
        .map(|b| x509::pem_encode("CERTIFICATE", &b.der))
        .collect()
}

/// The certificates of several trust bundles, deduplicated, in a stable
/// order (so an unchanged set rewrites the same file).
pub(crate) fn merge_bundles<'a>(bundles: impl IntoIterator<Item = &'a str>) -> String {
    let mut ders: Vec<Vec<u8>> = bundles
        .into_iter()
        .flat_map(|b| x509::pem_blocks(b).unwrap_or_default())
        .filter(|b| b.label == "CERTIFICATE")
        .map(|b| b.der)
        .collect();
    ders.sort();
    ders.dedup();
    ders.iter().map(|d| x509::pem_encode("CERTIFICATE", d)).collect()
}

fn time(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_str).and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp())
}

/// The certificate for the `podCertificate` projection `proj` (source
/// `source` of the pod, `<volume>-<index>`), asking for one when there is
/// none or it is due for refresh. Errors read like the kubelet's.
pub(crate) async fn ensure(api: &impl PcrApi, pod_dir: &Path, pod: &Value, source: &str, proj: &Value, now: i64) -> Result<Issued, String> {
    let ns = pod.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let signer = proj.get("signerName").and_then(Value::as_str).unwrap_or_default();
    let path = state_dir(pod_dir).join(format!("{source}.json"));
    let mut st = load(&path);
    let before = st.clone();
    if let Some(p) = st.pending.clone() {
        match api.get(ns, &p.name).await? {
            None => st.pending = None,
            Some(pcr) => match certificates::pcr_outcome(&pcr) {
                Ok(None) => {
                    if let Some(p) = st.pending.as_mut() {
                        p.checked_at = now;
                    }
                }
                Ok(Some(chain)) => {
                    let not_after = time(&pcr, "/status/notAfter").unwrap_or(now);
                    let begin = time(&pcr, "/status/beginRefreshAt").unwrap_or_else(|| now + (not_after - now) / 2);
                    st.current = Some(Issued { key_pem: p.key_pem, chain_pem: normalize_chain(&chain), not_after, begin_refresh_at: begin });
                    st.pending = None;
                    st.error = None;
                }
                Err(reason) => {
                    st.pending = None;
                    st.error = Some(format!("PodCertificateRequest {ns}/{} was not issued: {reason}", p.name));
                    st.retry_at = now + RETRY_SECS;
                }
            },
        }
    }
    let due = st.current.as_ref().is_none_or(|c| now >= c.begin_refresh_at);
    if due && st.pending.is_none() && now >= st.retry_at {
        let key_type = proj.get("keyType").and_then(Value::as_str).unwrap_or_default();
        let created = match new_key(key_type) {
            Ok((key_pem, csr)) => {
                use base64::Engine;
                let request = json!({
                    "podName": pod.pointer("/metadata/name"),
                    "source": source,
                    "signerName": signer,
                    "maxExpirationSeconds": proj.get("maxExpirationSeconds"),
                    "stubPKCS10Request": base64::engine::general_purpose::STANDARD.encode(csr),
                    "unverifiedUserAnnotations": proj.get("userAnnotations"),
                });
                api.create(ns, request).await.map(|pcr| (pcr, key_pem))
            }
            Err(e) => Err(e),
        };
        match created {
            Ok((pcr, key_pem)) => {
                let name = pcr.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
                st.pending = Some(Pending { name, key_pem, checked_at: now });
            }
            Err(e) => {
                st.error = Some(format!("failed to request a certificate from {signer}: {e}"));
                st.retry_at = now + RETRY_SECS;
            }
        }
    }
    if st != before {
        save(&path, &st)?;
    }
    match (&st.current, &st.pending) {
        (Some(c), _) if now < c.not_after => Ok(c.clone()),
        (_, Some(p)) => Err(format!("waiting for PodCertificateRequest {ns}/{} to be issued by {signer}", p.name)),
        _ => Err(st.error.clone().unwrap_or_else(|| format!("no certificate from {signer} yet"))),
    }
}

/// When the pod's certificates next need attention (a refresh, a pending
/// request to look at, a retry), in Unix seconds.
pub(crate) fn next_due(pod_dir: &Path) -> Option<i64> {
    let entries = std::fs::read_dir(state_dir(pod_dir)).ok()?;
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let st = load(&e.path());
            if let Some(p) = &st.pending {
                return Some(p.checked_at + POLL_SECS);
            }
            let refresh = st.current.as_ref().map(|c| c.begin_refresh_at);
            Some(refresh.unwrap_or(st.retry_at).max(st.retry_at))
        })
        .min()
}

/// Whether one of the pod's projections waits for the request `name`.
pub(crate) fn waits_for(pod_dir: &Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(state_dir(pod_dir)) else { return false };
    entries.flatten().any(|e| load(&e.path()).pending.is_some_and(|p| p.name == name))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    /// The API server plus a signer: requests are stored, and issued (or
    /// denied) when the test says so, with certificates from a test CA.
    struct Signer {
        ca: rcgen::Certificate,
        ca_key: rcgen::KeyPair,
        pcrs: RefCell<HashMap<String, Value>>,
        created: RefCell<u32>,
        lifetime: i64,
    }

    /// A public key for rcgen to sign, from the request's SPKI.
    struct Spki(x509::PublicKey);

    impl rcgen::PublicKeyData for Spki {
        fn der_bytes(&self) -> &[u8] {
            &self.0.key
        }
        fn algorithm(&self) -> &rcgen::SignatureAlgorithm {
            match self.0.key_type {
                x509::KeyType::EcP384 => &rcgen::PKCS_ECDSA_P384_SHA384,
                x509::KeyType::Ed25519 => &rcgen::PKCS_ED25519,
                _ => &rcgen::PKCS_ECDSA_P256_SHA256,
            }
        }
    }

    impl Signer {
        fn new(lifetime: i64) -> Self {
            let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
            let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            Self { ca: p.self_signed(&ca_key).unwrap(), ca_key, pcrs: RefCell::default(), created: RefCell::new(0), lifetime }
        }

        /// Issue every pending request, as a signer would through /status.
        fn issue_all(&self, now: i64) {
            let ts = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            for pcr in self.pcrs.borrow_mut().values_mut().filter(|p| p.get("status").is_none()) {
                let key = certificates::pcr_public_key(pcr).unwrap();
                let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
                p.not_before = time::OffsetDateTime::from_unix_timestamp(now).unwrap();
                p.not_after = time::OffsetDateTime::from_unix_timestamp(now + self.lifetime).unwrap();
                let leaf = p.signed_by(&Spki(key), &self.ca, &self.ca_key).unwrap();
                let old = pcr.clone();
                pcr["status"] = json!({
                    "conditions": [{"type": "Issued", "status": "True", "reason": "Issued", "lastTransitionTime": ts(now)}],
                    // Data between blocks, which the node drops.
                    "certificateChain": format!("{}comment\n{}", leaf.pem(), self.ca.pem()),
                    "notBefore": ts(now), "notAfter": ts(now + self.lifetime), "beginRefreshAt": ts(now + self.lifetime * 3 / 4),
                });
                assert_eq!(certificates::validate_pcr_status(pcr, &old, now), Vec::<String>::new());
            }
        }

        fn deny_all(&self) {
            for pcr in self.pcrs.borrow_mut().values_mut().filter(|p| p.get("status").is_none()) {
                pcr["status"] = json!({"conditions": [{"type": "Denied", "status": "True", "reason": "UnsupportedKeyType", "message": "no", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
            }
        }
    }

    impl PcrApi for Signer {
        async fn create(&self, ns: &str, request: Value) -> Result<Value, String> {
            *self.created.borrow_mut() += 1;
            let name = format!("worker-{}", self.created.borrow());
            let mut pcr = json!({"metadata": {"name": name, "namespace": ns}, "spec": request});
            certificates::default_pcr(&mut pcr);
            pcr["spec"]["podUID"] = json!("uid");
            for f in ["serviceAccountName", "serviceAccountUID", "nodeName", "nodeUID"] {
                pcr["spec"][f] = json!("x");
            }
            let errs = certificates::validate("PodCertificateRequest", &pcr, None);
            if !errs.is_empty() {
                return Err(errs.join(", "));
            }
            self.pcrs.borrow_mut().insert(name, pcr.clone());
            Ok(pcr)
        }

        async fn get(&self, _ns: &str, name: &str) -> Result<Option<Value>, String> {
            Ok(self.pcrs.borrow().get(name).cloned())
        }
    }

    fn pod() -> Value {
        json!({"metadata": {"name": "worker", "namespace": "default", "uid": "uid"}})
    }

    fn proj(key_type: &str) -> Value {
        json!({"signerName": "podidentity.podcert.ate.dev/identity", "keyType": key_type, "credentialBundlePath": "c.pem", "userAnnotations": {"ate.dev/a": "b"}})
    }

    #[tokio::test]
    async fn requests_waits_for_the_signer_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new(4 * 3600);
        let now = chrono::Utc::now().timestamp();
        // First pass: a request, nothing to write yet.
        let err = ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), now).await.unwrap_err();
        assert!(err.contains("waiting for PodCertificateRequest default/worker-1 to be issued by podidentity.podcert.ate.dev/identity"), "{err}");
        assert_eq!(next_due(dir.path()), Some(now + POLL_SECS));
        assert!(waits_for(dir.path(), "worker-1"));
        let req = signer.pcrs.borrow()["worker-1"].clone();
        assert_eq!(req["spec"]["unverifiedUserAnnotations"], json!({"ate.dev/a": "b"}));
        assert_eq!(req["spec"]["maxExpirationSeconds"], json!(86400));
        // Still pending: no second request.
        assert!(ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), now + 5).await.is_err());
        assert_eq!(*signer.created.borrow(), 1);

        signer.issue_all(now);
        let first = ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), now + 10).await.unwrap();
        assert!(first.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(!first.chain_pem.contains("comment"));
        assert_eq!(x509::pem_blocks(&first.chain_pem).unwrap().len(), 2);
        // The leaf is for our key.
        let leaf = x509::parse_certificate(&x509::pem_blocks(&first.chain_pem).unwrap()[0].der).unwrap();
        let key = x509::pem_blocks(&first.key_pem).unwrap()[0].der.clone();
        let kp = rcgen::KeyPair::try_from(key.as_slice()).unwrap();
        assert_eq!(leaf.public_key.spki, kp.public_key_der());
        assert_eq!(next_due(dir.path()), Some(first.begin_refresh_at));
        assert!(!waits_for(dir.path(), "worker-1"));

        // A restarted agent finds it on disk: no new request.
        assert_eq!(ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), now + 20).await.unwrap(), first);
        assert_eq!(*signer.created.borrow(), 1);

        // Refresh time: a new key is requested, the old certificate is still served.
        let later = first.begin_refresh_at + 1;
        assert_eq!(ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), later).await.unwrap(), first);
        assert_eq!(*signer.created.borrow(), 2);
        signer.issue_all(later);
        let second = ensure(&signer, dir.path(), &pod(), "identity-0", &proj("ECDSAP256"), later + 1).await.unwrap();
        assert_ne!(second.key_pem, first.key_pem);
        assert!(second.not_after > first.not_after);
    }

    #[tokio::test]
    async fn denied_requests_are_retried_later() {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new(4 * 3600);
        let now = chrono::Utc::now().timestamp();
        let _ = ensure(&signer, dir.path(), &pod(), "v-0", &proj("ED25519"), now).await;
        signer.deny_all();
        let err = ensure(&signer, dir.path(), &pod(), "v-0", &proj("ED25519"), now + 1).await.unwrap_err();
        assert!(err.contains("was not issued: Denied: UnsupportedKeyType: no"), "{err}");
        assert_eq!(*signer.created.borrow(), 1);
        assert_eq!(next_due(dir.path()), Some(now + 1 + RETRY_SECS));
        let _ = ensure(&signer, dir.path(), &pod(), "v-0", &proj("ED25519"), now + 2 + RETRY_SECS).await;
        assert_eq!(*signer.created.borrow(), 2);
    }

    #[tokio::test]
    async fn unsupported_key_types_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new(4 * 3600);
        let err = ensure(&signer, dir.path(), &pod(), "v-0", &proj("RSA3072"), 0).await.unwrap_err();
        assert!(err.contains("keyType RSA3072 is not supported on litek8s nodes yet"), "{err}");
        assert_eq!(*signer.created.borrow(), 0);
    }

    #[test]
    fn bundles_are_merged_and_deduplicated() {
        let a = x509::pem_encode("CERTIFICATE", b"a");
        let b = x509::pem_encode("CERTIFICATE", b"b");
        let merged = merge_bundles([format!("{b}junk\n{a}").as_str(), a.as_str()]);
        assert_eq!(merged, format!("{a}{b}"));
    }
}
