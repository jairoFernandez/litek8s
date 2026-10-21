//! The Kubernetes side: which API server (a running local dev cluster, or a
//! kubeconfig context), the pods Home counts, and the Kubernetes view: a
//! cluster UI for any cluster (see `state` for keys, `rt` for what it does).

pub mod auth;
pub mod command;
pub mod describe;
pub mod draw;
pub mod exec;
pub mod filter;
pub mod forward;
pub mod informer;
pub mod ops;
pub mod resources;
pub mod rt;
pub mod state;
pub mod table;
pub mod treeview;
pub mod uiconfig;
pub mod yaml;

pub use state::{KEffect, KMsg, KubeUi};

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use hyper::Method;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::app::Msg;
use super::client::{Endpoint, Lines, enc};
use super::model;

/// The API server to use: `context` from the kubeconfig if given; else the
/// local dev cluster if it runs; else the kubeconfig's `litek8s-dev`
/// context (`lkdev-dev` with `$LITEK8S_HOME`), else its current one.
pub fn resolve(context: Option<&str>) -> Result<(Endpoint, String)> {
    let default = crate::dev::kubeconfig::default_context();
    if context.is_none()
        && let Some(state) = crate::dev::running_state()
    {
        let ep = Endpoint::parse_http(&format!("http://127.0.0.1:{}", state.port), None)?;
        return Ok((ep, default));
    }
    let cfg = auth::load()?;
    let name = match context {
        Some(c) => c.to_string(),
        None if cfg.named("contexts", &default).is_some() => default,
        None => cfg.current().unwrap_or_default(),
    };
    Ok((endpoint_for(&cfg.doc, &name, Path::new("."))?, name))
}

fn named<'a>(cfg: &'a serde_yaml::Value, list: &str, name: &str) -> Option<&'a serde_yaml::Value> {
    cfg[list].as_sequence()?.iter().find(|e| e["name"].as_str() == Some(name))
}

/// Inline `<key>-data` (base64) or the file `<key>` (relative to the
/// kubeconfig's directory).
fn data_or_file(entry: &serde_yaml::Value, key: &str, dir: &Path) -> Result<Option<Vec<u8>>> {
    let dir = &entry["x-litek8s-dir"].as_str().map(std::path::PathBuf::from).unwrap_or_else(|| dir.to_path_buf());
    if let Some(d) = entry[format!("{key}-data").as_str()].as_str() {
        return Ok(Some(base64::engine::general_purpose::STANDARD.decode(d.trim()).with_context(|| format!("{key}-data is not base64"))?));
    }
    if let Some(f) = entry[key].as_str() {
        let p = dir.join(f);
        return Ok(Some(std::fs::read(&p).with_context(|| format!("cannot read {}", p.display()))?));
    }
    Ok(None)
}

pub fn endpoint_for(cfg: &serde_yaml::Value, context: &str, dir: &Path) -> Result<Endpoint> {
    let ctx = named(cfg, "contexts", context).with_context(|| format!("no context {context:?} in the kubeconfig"))?;
    let cluster_name = ctx["context"]["cluster"].as_str().unwrap_or_default();
    let user_name = ctx["context"]["user"].as_str().unwrap_or_default();
    let cluster_entry = named(cfg, "clusters", cluster_name).with_context(|| format!("no cluster {cluster_name:?} in the kubeconfig"))?;
    let null = serde_yaml::Value::Null;
    let user_entry = named(cfg, "users", user_name).unwrap_or(&null);
    // Files named in an entry are relative to the kubeconfig that has it.
    let mut cluster = cluster_entry["cluster"].clone();
    let mut user = user_entry["user"].clone();
    for (e, from) in [(&mut cluster, cluster_entry), (&mut user, user_entry)] {
        if let (serde_yaml::Value::Mapping(m), Some(d)) = (e, from["x-litek8s-dir"].as_str()) {
            m.insert("x-litek8s-dir".into(), d.into());
        }
    }
    let (cluster, user) = (&cluster, &user);
    let server = cluster["server"].as_str().context("the cluster has no server")?;
    let creds = auth::dynamic(user_entry, dir).with_context(|| format!("context {context}"))?;
    let tls = if server.starts_with("https://") {
        let insecure = cluster["insecure-skip-tls-verify"].as_bool().unwrap_or(false);
        let ca = data_or_file(cluster, "certificate-authority", dir)?;
        let cert = data_or_file(user, "client-certificate", dir)?;
        let key = data_or_file(user, "client-key", dir)?;
        if insecure && cert.is_none() { None } else { Some(tls_config(ca, cert.zip(key), insecure)?) }
    } else {
        None
    };
    let mut ep = Endpoint::parse_http(server, tls)?;
    ep.creds = creds;
    let user_dir = user["x-litek8s-dir"].as_str().map(std::path::PathBuf::from).unwrap_or_else(|| dir.to_path_buf());
    let token = match (user["token"].as_str(), user["tokenFile"].as_str()) {
        (Some(t), _) => Some(t.to_string()),
        (None, Some(f)) => Some(std::fs::read_to_string(user_dir.join(f))?.trim().to_string()),
        _ => None,
    };
    if let Some(t) = token {
        ep.headers.push(("Authorization".into(), format!("Bearer {t}")));
    } else if let (Some(u), Some(p)) = (user["username"].as_str(), user["password"].as_str()) {
        ep.headers.push(("Authorization".into(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))));
    }
    Ok(ep)
}

fn tls_config(ca: Option<Vec<u8>>, client: Option<(Vec<u8>, Vec<u8>)>, insecure: bool) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let builder = if insecure {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        match ca {
            Some(pem) => {
                for cert in CertificateDer::pem_slice_iter(&pem) {
                    roots.add(cert?)?;
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        builder.with_root_certificates(roots)
    };
    let mut config = match client {
        Some((cert, key)) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert).collect::<Result<_, _>>()?;
            builder.with_client_auth_cert(certs, PrivateKeyDer::from_pem_slice(&key)?)?
        }
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Keep the pods Home counts (and names cgroups and images by) current:
/// a list, then a watch from its resourceVersion; re-list when the watch
/// ends. Only what `model::pod` reads is kept of each pod.
pub fn spawn_watches(ep: Endpoint, tx: UnboundedSender<Msg>) -> Vec<tokio::task::JoinHandle<()>> {
    vec![tokio::spawn(watch_pods(ep, tx))]
}

fn key_of(o: &Value) -> String {
    format!("{}/{}", o.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(""), o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or(""))
}

/// A pod with only the fields [`model::pod`] reads: lists are read
/// straight into these, so a cluster's pods never sit in memory whole.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct PodLite {
    metadata: PodMeta,
    spec: PodSpec,
    status: PodStatus,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PodMeta {
    name: String,
    namespace: String,
    uid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    creation_timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deletion_timestamp: Option<String>,
    #[serde(serialize_with = "first_owner")]
    owner_references: Vec<Named>,
}

fn first_owner<S: serde::Serializer>(v: &[Named], s: S) -> std::result::Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&v[..v.len().min(1)], s)
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct Named {
    name: String,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PodSpec {
    containers: Vec<ContainerLite>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_name: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct ContainerLite {
    name: String,
    image: String,
    ports: Vec<PortLite>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PortLite {
    container_port: u64,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
struct PodStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    container_statuses: Vec<StatusLite>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
struct StatusLite {
    ready: bool,
    restart_count: i64,
    #[serde(rename = "imageID")]
    image_id: String,
    state: Value,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct PodList {
    metadata: table::ListMeta,
    items: Vec<PodLite>,
}

/// A pod with only the fields [`model::pod`] reads.
pub fn slim_pod(p: &Value) -> Value {
    PodLite::deserialize(p).ok().and_then(|l| serde_json::to_value(l).ok()).unwrap_or(Value::Null)
}

async fn watch_pods(ep: Endpoint, tx: UnboundedSender<Msg>) {
    const PATH: &str = "/api/v1/pods";
    let msg = |items: &BTreeMap<String, model::Pod>| Msg::Pods({
        let mut v: Vec<model::Pod> = items.values().cloned().collect();
        v.sort_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)));
        v
    });
    let mut backoff = Duration::from_secs(1);
    loop {
        // Pages read straight into slim pods.
        let mut items: BTreeMap<String, model::Pod> = BTreeMap::new();
        let mut cont = String::new();
        let listed: Result<String> = async {
            loop {
                let q = if cont.is_empty() { format!("{PATH}?limit=2000") } else { format!("{PATH}?limit=2000&continue={}", enc(&cont)) };
                let page: PodList = ep.json_accept(&q, "application/json").await?;
                for p in page.items {
                    let v = serde_json::to_value(&p)?;
                    items.insert(format!("{}/{}", p.metadata.namespace, p.metadata.name), model::pod(&v));
                }
                if page.metadata.cont.is_empty() {
                    return Ok(page.metadata.resource_version);
                }
                cont = page.metadata.cont;
            }
        }
        .await;
        let rv = match listed {
            Ok(rv) => rv,
            Err(e) => {
                if tx.send(Msg::KubeDown(format!("{}: {e:#}", ep.display))).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        if tx.send(msg(&items)).is_err() {
            return;
        }
        let path = format!("{PATH}?watch=1&allowWatchBookmarks=true&resourceVersion={}", enc(&rv));
        let Ok(resp) = ep.send(Method::GET, &path, None, Vec::new()).await else {
            tokio::time::sleep(backoff).await;
            continue;
        };
        if !resp.status().is_success() {
            tokio::time::sleep(backoff).await;
            continue;
        }
        let mut lines = Lines::new(resp.into_body());
        let mut dirty = false;
        let mut since = std::time::Instant::now();
        loop {
            // Home counts pods: 500 ms of quiet, or every 2 s of a steady
            // stream, is soon enough (and a cluster of thousands of pods
            // is not converted again for every change).
            let wait = Duration::from_millis(500).min(Duration::from_secs(2).saturating_sub(since.elapsed()));
            let next = if dirty { tokio::time::timeout(wait, lines.next()).await } else { Ok(lines.next().await) };
            match next {
                Err(_) => {
                    dirty = false;
                    if tx.send(msg(&items)).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Ok(Some(line)) => {
                    let Ok(ev) = serde_json::from_slice::<Value>(&line) else { continue };
                    let obj = &ev["object"];
                    if !dirty {
                        since = std::time::Instant::now();
                    }
                    match ev["type"].as_str() {
                        Some("ADDED") | Some("MODIFIED") => {
                            items.insert(key_of(obj), model::pod(&slim_pod(obj)));
                            dirty = true;
                        }
                        Some("DELETED") => {
                            items.remove(&key_of(obj));
                            dirty = true;
                        }
                        Some("ERROR") => break,
                        _ => {}
                    }
                }
            }
        }
        if dirty {
            let _ = tx.send(msg(&items));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_from_a_kubeconfig() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "s3cret\n").unwrap();
        let cfg: serde_yaml::Value = serde_yaml::from_str(
            "clusters:\n- name: dev\n  cluster: {server: 'http://127.0.0.1:6443'}\n- name: far\n  cluster: {server: 'https://far.example:6443', insecure-skip-tls-verify: true}\n\
             contexts:\n- name: litek8s-dev\n  context: {cluster: dev, user: dev}\n- name: far\n  context: {cluster: far, user: far}\n- name: gke\n  context: {cluster: far, user: gke}\n\
             users:\n- name: dev\n  user: {}\n- name: far\n  user: {tokenFile: token}\n- name: gke\n  user: {exec: {command: gke-gcloud-auth-plugin}}\n",
        )
        .unwrap();
        let ep = endpoint_for(&cfg, "litek8s-dev", dir.path()).unwrap();
        assert_eq!(ep.display, "http://127.0.0.1:6443");
        assert!(ep.headers.is_empty());
        let ep = endpoint_for(&cfg, "far", dir.path()).unwrap();
        assert_eq!(ep.headers, [("Authorization".to_string(), "Bearer s3cret".to_string())]);
        assert!(matches!(ep.transport, super::super::client::Transport::Tls { config: None, .. }));
        // Exec plugins give tokens per request.
        let ep = endpoint_for(&cfg, "gke", dir.path()).unwrap();
        assert!(ep.creds.is_some() && ep.headers.is_empty());
        assert!(endpoint_for(&cfg, "nope", dir.path()).is_err());
    }
}

#[cfg(test)]
mod view_tests;
