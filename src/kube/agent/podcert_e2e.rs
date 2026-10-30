//! A pod with `podCertificate` and `clusterTrustBundle` projections, run by
//! a real agent against an in-process API server, with a runtime that only
//! records what it would run and a test signer issuing the requests
//! through `/status` (docs/specs/09-pod-certificates.md).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};

use super::runtime::*;
use super::{Agent, AgentConfig};
use crate::kube::{ApiState, certificates, x509};

/// Containers "run" forever; their specs are kept for the test to look at.
#[derive(Default)]
struct FakeRuntime {
    containers: Mutex<HashMap<String, ContainerSpec>>,
}

impl PodRuntime for FakeRuntime {
    async fn machine(&self) -> Result<MachineInfo> {
        Ok(MachineInfo { cpus: 2, memory: 4 << 30, arch: "amd64".into(), os: "linux".into(), ..Default::default() })
    }
    async fn pull_image(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn ensure_volume(&self, _: &str, _: HashMap<String, String>) -> Result<bool> {
        Ok(true)
    }
    async fn remove_volume(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn list_volumes(&self, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn run_sandbox(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        Ok(SandboxInfo { id: format!("sb-{}", spec.name), ip: Some("10.42.0.2".into()) })
    }
    async fn sandbox_ip(&self, _: &str) -> Result<Option<String>> {
        Ok(Some("10.42.0.2".into()))
    }
    async fn prepare_sandbox_dirs(&self, _: &str, _: Vec<String>, _: Vec<String>) -> Result<()> {
        Ok(())
    }
    async fn run_container(&self, _: &str, spec: &ContainerSpec) -> Result<String> {
        let id = format!("c-{}", spec.name);
        self.containers.lock().unwrap().insert(id.clone(), spec.clone());
        Ok(id)
    }
    async fn inspect(&self, id: &str) -> Result<Option<ContainerInfo>> {
        let known = self.containers.lock().unwrap().contains_key(id);
        Ok(known.then(|| ContainerInfo { running: true, started_at: Some("2026-01-01T00:00:00Z".into()), ..Default::default() }))
    }
    async fn stop(&self, _: &str, _: Duration) -> Result<()> {
        Ok(())
    }
    async fn remove(&self, id: &str) -> Result<()> {
        self.containers.lock().unwrap().remove(id);
        Ok(())
    }
    async fn list(&self) -> Result<Vec<RuntimeContainer>> {
        Ok(Vec::new())
    }
    async fn exec(&self, _: &str, _: Vec<String>, _: bool, _: bool) -> Result<ExecSession> {
        bail!("no exec in the fake runtime")
    }
    async fn exec_exit_code(&self, _: &str) -> Result<Option<i64>> {
        Ok(None)
    }
    async fn resize_exec(&self, _: &str, _: u16, _: u16) -> Result<()> {
        Ok(())
    }
    fn logs(&self, _: &str, _: LogOptions) -> ByteStream {
        futures_util::stream::empty().boxed()
    }
    async fn stats(&self, _: &str) -> Result<Option<(u64, u64)>> {
        Ok(None)
    }
    async fn apply_service_rules(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn host_gateway_ip(&self) -> Result<String> {
        Ok("10.42.0.1".into())
    }
    fn exits(&self) -> BoxStream<'static, String> {
        futures_util::stream::pending().boxed()
    }
}

/// The SubjectPublicKeyInfo of a request, for rcgen to sign.
struct Spki(x509::PublicKey);

impl rcgen::PublicKeyData for Spki {
    fn der_bytes(&self) -> &[u8] {
        &self.0.key
    }
    fn algorithm(&self) -> &rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

fn ca(name: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    p.distinguished_name.push(rcgen::DnType::CommonName, name);
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    (p.self_signed(&key).unwrap(), key)
}

/// Sign every pending request for `signer` through `/status`, like an
/// external signer controller would.
fn sign_pending(api: &ApiState, signer: &str, ca: &rcgen::Certificate, ca_key: &rcgen::KeyPair) -> usize {
    let ts = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (pcrs, _) = api.store().list(certificates::GROUP, "podcertificaterequests", None);
    let mut n = 0;
    for mut pcr in pcrs.into_iter().filter(|p| p.pointer("/spec/signerName") == Some(&json!(signer)) && p.get("status").is_none_or(|s| s.get("conditions").is_none())) {
        let now = chrono::Utc::now().timestamp();
        let key = certificates::pcr_public_key(&pcr).unwrap();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.not_before = time::OffsetDateTime::from_unix_timestamp(now).unwrap();
        p.not_after = time::OffsetDateTime::from_unix_timestamp(now + 6 * 3600).unwrap();
        let leaf = p.signed_by(&Spki(key), ca, ca_key).unwrap();
        pcr["status"] = json!({
            "conditions": [{"type": "Issued", "status": "True", "reason": "Issued", "message": "test signer", "lastTransitionTime": ts(now)}],
            "certificateChain": leaf.pem(),
            "notBefore": ts(now), "notAfter": ts(now + 6 * 3600), "beginRefreshAt": ts(now + 4 * 3600),
        });
        api.update_status(certificates::GROUP, "podcertificaterequests", pcr).unwrap();
        n += 1;
    }
    n
}

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(v) = check() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The files the container sees at `target`.
fn mounted(rt: &FakeRuntime, target: &str) -> Option<std::path::PathBuf> {
    rt.containers.lock().unwrap().values().flat_map(|c| c.mounts.clone()).find_map(|m| match m.source {
        MountSource::Bind(src) if m.target == target => Some(std::path::PathBuf::from(src)),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pod_gets_its_certificate_and_trust_bundle() {
    let api = Arc::new(ApiState::open(None, "127.0.0.1:6443".into(), false).unwrap());
    let root = tempfile::tempdir().unwrap();
    let agent = Agent::new(
        FakeRuntime::default(),
        AgentConfig { node_name: "n1".into(), root_dir: root.path().to_path_buf(), node: json!({"metadata": {"name": "n1"}}), node_ip: "10.0.0.5".into() },
    );
    let rt = agent.runtime().clone();
    tokio::spawn(super::run_local(api.clone(), agent, 6443));
    eventually("the node", || api.get_object("", "nodes", "", "n1")).await;

    const SIGNER: &str = "podidentity.podcert.ate.dev/identity";
    let (ca1, ca1_key) = ca("ca-1");
    api.create_object(certificates::GROUP, "clustertrustbundles", None, json!({
        "metadata": {"name": "podidentity.podcert.ate.dev:identity:one", "labels": {"podcert.ate.dev/canarying": "live"}},
        "spec": {"signerName": SIGNER, "trustBundle": ca1.pem()},
    }))
    .unwrap();
    api.create_object("", "serviceaccounts", Some("default"), json!({"metadata": {"name": "worker"}})).unwrap();
    // What the root CA publisher would have made (the service account volume).
    api.create_object("", "configmaps", Some("default"), json!({"metadata": {"name": "kube-root-ca.crt"}, "data": {"ca.crt": ""}})).unwrap();
    // Substrate's worker volume, as its controller writes it.
    api.create_object("", "pods", Some("default"), json!({
        "metadata": {"name": "worker"},
        "spec": {
            "nodeName": "n1", "serviceAccountName": "worker",
            "containers": [{"name": "atelet", "image": "ate/atelet", "volumeMounts": [{"name": "identity", "mountPath": "/var/run/identity", "readOnly": true}]}],
            "volumes": [{"name": "identity", "projected": {"sources": [
                {"podCertificate": {"signerName": SIGNER, "keyType": "ECDSAP256", "credentialBundlePath": "credential-bundle.pem", "keyPath": "key.pem", "certificateChainPath": "chain.pem"}},
                {"clusterTrustBundle": {"signerName": SIGNER, "labelSelector": {"matchLabels": {"podcert.ate.dev/canarying": "live"}}, "path": "trust-bundle.pem"}},
                {"clusterTrustBundle": {"name": "egress-mitm", "path": "egress.pem", "optional": true}},
            ]}}],
        },
    }))
    .unwrap();

    // The node asks; the pod waits for the signer.
    let pcr = eventually("a PodCertificateRequest", || {
        let (pcrs, _) = api.store().list(certificates::GROUP, "podcertificaterequests", Some("default"));
        pcrs.into_iter().next()
    })
    .await;
    let pod = api.get_object("", "pods", "default", "worker").unwrap();
    assert_eq!(pcr["spec"]["podUID"], pod["metadata"]["uid"]);
    assert_eq!(pcr["spec"]["serviceAccountName"], json!("worker"));
    assert_eq!(pcr["metadata"]["ownerReferences"][0]["uid"], pod["metadata"]["uid"]);
    let waiting = eventually("the pod to say it waits", || {
        let pod = api.get_object("", "pods", "default", "worker")?;
        let msg = pod.pointer("/status/containerStatuses/0/state/waiting/message").and_then(Value::as_str)?.to_string();
        msg.contains("waiting for PodCertificateRequest").then_some(msg)
    })
    .await;
    assert!(waiting.contains(SIGNER), "{waiting}");
    assert!(mounted(&rt, "/var/run/identity").is_none());

    // The signer issues: the container starts with its files.
    assert_eq!(sign_pending(&api, SIGNER, &ca1, &ca1_key), 1);
    let dir = eventually("the container", || mounted(&rt, "/var/run/identity")).await;
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let bundle = read("credential-bundle.pem");
    let blocks = x509::pem_blocks(&bundle).unwrap();
    assert_eq!(blocks.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(), ["PRIVATE KEY", "CERTIFICATE"]);
    let leaf = x509::parse_certificate(&blocks[1].der).unwrap();
    let key = rcgen::KeyPair::try_from(blocks[0].der.as_slice()).unwrap();
    assert_eq!(leaf.public_key.spki, key.public_key_der(), "the certificate is for the pod's key");
    assert_eq!(read("key.pem") + &read("chain.pem"), bundle);
    assert_eq!(read("trust-bundle.pem"), x509::pem_encode("CERTIFICATE", ca1.der()));
    assert!(!dir.join("egress.pem").exists(), "an optional bundle that does not exist writes nothing");

    // A second CA for the signer (rotation): the trust bundle follows.
    let (ca2, _) = ca("ca-2");
    api.create_object(certificates::GROUP, "clustertrustbundles", None, json!({
        "metadata": {"name": "podidentity.podcert.ate.dev:identity:two", "labels": {"podcert.ate.dev/canarying": "live"}},
        "spec": {"signerName": SIGNER, "trustBundle": ca2.pem()},
    }))
    .unwrap();
    let bundle = eventually("the second CA in the trust bundle", || {
        let b = read("trust-bundle.pem");
        (x509::pem_blocks(&b).ok()?.len() == 2).then_some(b)
    })
    .await;
    assert!(bundle.contains(&x509::pem_encode("CERTIFICATE", ca2.der())));
    // The certificate was not asked for again.
    let (pcrs, _) = api.store().list(certificates::GROUP, "podcertificaterequests", Some("default"));
    assert_eq!(pcrs.len(), 1);
}
