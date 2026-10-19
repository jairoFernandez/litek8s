//! The Kubernetes side: which API server (a running local dev cluster, or a
//! kubeconfig context), list+watch of pods, deployments and namespaces,
//! logs, scale, restart, delete and port-forward.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use hyper::Method;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::app::{Forward, Msg};
use super::client::{Endpoint, Lines, enc};
use super::model;

/// The API server to use: `context` from the kubeconfig if given; else the
/// local dev cluster if it runs; else the kubeconfig's `litek8s-dev`
/// context, else its current one.
pub fn resolve(context: Option<&str>) -> Result<(Endpoint, String)> {
    let path = crate::dev::kubeconfig::default_path();
    if context.is_none() {
        if let Some(state) = crate::dev::running_state() {
            let ep = Endpoint::parse_http(&format!("http://127.0.0.1:{}", state.port), None)?;
            return Ok((ep, crate::dev::kubeconfig::CONTEXT.into()));
        }
    }
    let doc = std::fs::read_to_string(&path).with_context(|| format!("no kubeconfig at {}", path.display()))?;
    let cfg: serde_yaml::Value = serde_yaml::from_str(&doc).with_context(|| format!("{} is not valid YAML", path.display()))?;
    let name = match context {
        Some(c) => c.to_string(),
        None if named(&cfg, "contexts", crate::dev::kubeconfig::CONTEXT).is_some() => crate::dev::kubeconfig::CONTEXT.into(),
        None => cfg["current-context"].as_str().unwrap_or_default().to_string(),
    };
    Ok((endpoint_for(&cfg, &name, path.parent().unwrap_or(Path::new(".")))?, name))
}

fn named<'a>(cfg: &'a serde_yaml::Value, list: &str, name: &str) -> Option<&'a serde_yaml::Value> {
    cfg[list].as_sequence()?.iter().find(|e| e["name"].as_str() == Some(name))
}

/// Inline `<key>-data` (base64) or the file `<key>` (relative to the
/// kubeconfig's directory).
fn data_or_file(entry: &serde_yaml::Value, key: &str, dir: &Path) -> Result<Option<Vec<u8>>> {
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
    let cluster = &named(cfg, "clusters", cluster_name).with_context(|| format!("no cluster {cluster_name:?} in the kubeconfig"))?["cluster"];
    let null = serde_yaml::Value::Null;
    let user = named(cfg, "users", user_name).map(|u| &u["user"]).unwrap_or(&null);
    let server = cluster["server"].as_str().context("the cluster has no server")?;
    if !user["exec"].is_null() || !user["auth-provider"].is_null() {
        bail!("context {context}: exec and auth-provider credentials are not supported (k9s or kubectl handle them)");
    }
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
    let token = match (user["token"].as_str(), user["tokenFile"].as_str()) {
        (Some(t), _) => Some(t.to_string()),
        (None, Some(f)) => Some(std::fs::read_to_string(dir.join(f))?.trim().to_string()),
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

#[derive(Clone, Copy)]
enum Kind {
    Pods,
    Deployments,
    Namespaces,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Pods => "/api/v1/pods",
            Kind::Deployments => "/apis/apps/v1/deployments",
            Kind::Namespaces => "/api/v1/namespaces",
        }
    }

    fn msg(self, list: &Value) -> Msg {
        match self {
            Kind::Pods => Msg::Pods(model::pods(list)),
            Kind::Deployments => Msg::Deployments(model::deployments(list)),
            Kind::Namespaces => Msg::Namespaces(model::names(list)),
        }
    }
}

/// Keep the UI's pods, deployments and namespaces current: a list, then a
/// watch from its resourceVersion; re-list when the watch ends.
pub fn spawn_watches(ep: Endpoint, tx: UnboundedSender<Msg>) -> Vec<tokio::task::JoinHandle<()>> {
    [Kind::Pods, Kind::Deployments, Kind::Namespaces].into_iter().map(|k| tokio::spawn(watch(ep.clone(), k, tx.clone()))).collect()
}

fn key_of(o: &Value) -> String {
    format!("{}/{}", o.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(""), o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or(""))
}

async fn watch(ep: Endpoint, kind: Kind, tx: UnboundedSender<Msg>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let list = match ep.json(Method::GET, kind.path(), None).await {
            Ok(l) => l,
            Err(e) => {
                if matches!(kind, Kind::Pods) && tx.send(Msg::KubeDown(format!("{}: {e:#}", ep.display))).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        let mut items: BTreeMap<String, Value> = list["items"].as_array().into_iter().flatten().map(|o| (key_of(o), o.clone())).collect();
        let snapshot = |items: &BTreeMap<String, Value>| json!({"items": items.values().collect::<Vec<_>>()});
        if tx.send(kind.msg(&snapshot(&items))).is_err() {
            return;
        }
        let rv = list.pointer("/metadata/resourceVersion").and_then(Value::as_str).unwrap_or("0").to_string();
        let path = format!("{}?watch=1&allowWatchBookmarks=true&resourceVersion={}", kind.path(), enc(&rv));
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
        loop {
            // Coalesce bursts (a rollout changes many pods at once).
            let next = if dirty { tokio::time::timeout(Duration::from_millis(100), lines.next()).await } else { Ok(lines.next().await) };
            match next {
                Err(_) => {
                    dirty = false;
                    if tx.send(kind.msg(&snapshot(&items))).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Ok(Some(line)) => {
                    let Ok(ev) = serde_json::from_slice::<Value>(&line) else { continue };
                    let obj = &ev["object"];
                    match ev["type"].as_str() {
                        Some("ADDED") | Some("MODIFIED") => {
                            items.insert(key_of(obj), obj.clone());
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
            let _ = tx.send(kind.msg(&snapshot(&items)));
        }
    }
}

pub async fn scale(ep: &Endpoint, ns: &str, name: &str, replicas: i64) -> Result<String> {
    let path = format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}");
    ep.json_typed(Method::PATCH, &path, "application/merge-patch+json", Some(&json!({"spec": {"replicas": replicas}}))).await?;
    Ok(format!("{ns}/{name} scaled to {replicas}"))
}

pub async fn restart(ep: &Endpoint, ns: &str, name: &str) -> Result<String> {
    let path = format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let patch = json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": now}}}}});
    ep.json_typed(Method::PATCH, &path, "application/merge-patch+json", Some(&patch)).await?;
    Ok(format!("{ns}/{name} restarting"))
}

pub async fn delete_pod(ep: &Endpoint, ns: &str, name: &str) -> Result<String> {
    ep.json(Method::DELETE, &format!("/api/v1/namespaces/{ns}/pods/{name}"), None).await?;
    Ok(format!("pod {ns}/{name} deleted"))
}

/// Follow a pod's log; lines go out tagged with `id`.
pub async fn logs(ep: Endpoint, ns: String, name: String, container: Option<String>, id: u64, tx: UnboundedSender<Msg>) {
    let mut path = format!("/api/v1/namespaces/{ns}/pods/{name}/log?follow=true&tailLines=1000");
    if let Some(c) = container {
        path.push_str(&format!("&container={}", enc(&c)));
    }
    let result: Result<()> = async {
        let resp = ep.send(Method::GET, &path, None, Vec::new()).await?;
        if !resp.status().is_success() {
            let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
            let msg = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
            bail!("{msg}");
        }
        super::docker::pump_lines(Lines::new(resp.into_body()), false, id, &tx).await;
        Ok(())
    }
    .await;
    let _ = tx.send(Msg::LogEnd(id, result.err().map(|e| format!("{e:#}"))));
}

/// Active port-forwards: listeners on 127.0.0.1 relaying each connection
/// through `pods/<pod>/portforward`.
#[derive(Default, Clone)]
pub struct Forwards {
    inner: Arc<Mutex<Vec<(Forward, tokio::task::JoinHandle<()>)>>>,
}

impl Forwards {
    pub fn list(&self) -> Vec<Forward> {
        let mut list = self.inner.lock().unwrap();
        list.retain(|(_, t)| !t.is_finished());
        list.iter().map(|(f, _)| f.clone()).collect()
    }

    pub fn stop_all(&self) {
        for (_, t) in self.inner.lock().unwrap().drain(..) {
            t.abort();
        }
    }

    pub async fn start(&self, ep: Endpoint, ns: String, pod: String, local: u16, remote: u16) -> Result<String> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", local)).await.with_context(|| format!("cannot listen on 127.0.0.1:{local}"))?;
        // Fail now, not on the first connection, if the pod is not there.
        ep.json(Method::GET, &format!("/api/v1/namespaces/{ns}/pods/{pod}"), None).await?;
        let fwd = Forward { local, namespace: ns.clone(), pod: pod.clone(), remote };
        let path = format!("/api/v1/namespaces/{ns}/pods/{pod}/portforward?ports={remote}");
        let task = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let ep = ep.clone();
                let path = path.clone();
                tokio::spawn(async move {
                    let extra = [("X-Stream-Protocol-Version", "portforward.k8s.io")];
                    if let Ok((_, io)) = ep.upgrade(Method::POST, &path, "SPDY/3.1", &extra, None).await {
                        let _ = crate::kube::spdy::forward_one(io, remote, conn).await;
                    }
                });
            }
        });
        self.inner.lock().unwrap().push((fwd, task));
        Ok(format!("forwarding 127.0.0.1:{local} to {ns}/{pod}:{remote}"))
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
        assert!(endpoint_for(&cfg, "gke", dir.path()).unwrap_err().to_string().contains("exec"));
        assert!(endpoint_for(&cfg, "nope", dir.path()).is_err());
    }
}
