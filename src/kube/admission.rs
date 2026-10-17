//! Dynamic admission: calling the webhooks registered through
//! MutatingWebhookConfiguration and ValidatingWebhookConfiguration objects
//! (cert-manager, kyverno, ... rely on them).
//!
//! Supported: rules (operations, groups, versions, resources/subresources,
//! scope), namespace and object selectors, `failurePolicy`, `timeoutSeconds`,
//! service and URL client configs, JSONPatch responses. `matchConditions`
//! (CEL) are not evaluated: a webhook whose rules match is called.
//! Requests made by the in-process controllers skip webhooks for now.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};

use super::controllers::util;
use super::patch;
use super::registry::ResourceType;
use super::selector::LabelSelector;
use super::server::{ApiError, ApiState};

pub(crate) struct Attributes<'a> {
    pub rt: &'a ResourceType,
    pub operation: &'static str,
    pub namespace: &'a str,
    pub name: &'a str,
    pub sub: Option<&'a str>,
    pub old: Option<&'a Value>,
    pub dry_run: bool,
}

fn strings(v: Option<&Value>) -> Vec<&str> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn rule_matches(rule: &Value, a: &Attributes) -> bool {
    let any = |list: Vec<&str>, want: &str| list.iter().any(|x| *x == "*" || *x == want);
    if !any(strings(rule.get("operations")), a.operation) {
        return false;
    }
    if !any(strings(rule.get("apiGroups")), &a.rt.group) || !any(strings(rule.get("apiVersions")), &a.rt.version) {
        return false;
    }
    let resource = match a.sub {
        Some(sub) => format!("{}/{sub}", a.rt.plural),
        None => a.rt.plural.clone(),
    };
    let resource_ok = strings(rule.get("resources")).iter().any(|r| {
        *r == resource
            || (*r == "*" && a.sub.is_none())
            || *r == "*/*"
            || r.strip_suffix("/*").is_some_and(|base| a.sub.is_some() && base == a.rt.plural)
            || r.strip_prefix("*/").is_some_and(|sub| a.sub == Some(sub))
    });
    if !resource_ok {
        return false;
    }
    match rule.get("scope").and_then(Value::as_str).unwrap_or("*") {
        "Cluster" => !a.rt.namespaced,
        "Namespaced" => a.rt.namespaced,
        _ => true,
    }
}

fn webhook_matches(api: &ApiState, hook: &Value, a: &Attributes, obj: &Value) -> bool {
    let rules = hook.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
    if !rules.iter().any(|r| rule_matches(r, a)) {
        return false;
    }
    if let Some(sel) = hook.get("objectSelector").filter(|s| !s.is_null()) {
        let selector = LabelSelector::from_object(Some(sel)).unwrap_or_default();
        let matched = selector.matches(obj) || a.old.is_some_and(|o| selector.matches(o));
        if !matched {
            return false;
        }
    }
    if let Some(sel) = hook.get("namespaceSelector").filter(|s| !s.is_null()) {
        let selector = LabelSelector::from_object(Some(sel)).unwrap_or_default();
        if !selector.is_empty() {
            let ns_obj = if a.rt.group.is_empty() && a.rt.plural == "namespaces" {
                Some(obj.clone())
            } else if a.rt.namespaced {
                api.get_object("", "namespaces", "", a.namespace)
            } else {
                // Cluster-scoped objects are not filtered by namespace.
                return true;
            };
            let mut ns_obj = ns_obj.unwrap_or_else(|| json!({"metadata": {"name": a.namespace}}));
            // Every namespace carries its name as a label.
            if !ns_obj["metadata"].get("labels").is_some_and(Value::is_object) {
                ns_obj["metadata"]["labels"] = json!({});
            }
            let ns_name = util::name(&ns_obj);
            ns_obj["metadata"]["labels"]["kubernetes.io/metadata.name"] = json!(ns_name);
            if !selector.matches(&ns_obj) {
                return false;
            }
        }
    }
    true
}

/// Run mutating then validating webhooks on `obj`; returns the (possibly
/// patched) object.
pub(crate) async fn admit(api: &Arc<ApiState>, a: &Attributes<'_>, mut obj: Value) -> Result<Value, ApiError> {
    // Webhook configurations themselves are never subject to webhooks.
    if a.rt.group == "admissionregistration.k8s.io" {
        return Ok(obj);
    }
    for (plural, mutating) in [("mutatingwebhookconfigurations", true), ("validatingwebhookconfigurations", false)] {
        let (mut configs, _) = api.store().list("admissionregistration.k8s.io", plural, None);
        if configs.is_empty() {
            continue;
        }
        configs.sort_by_key(util::name);
        for config in &configs {
            for hook in config.get("webhooks").and_then(Value::as_array).into_iter().flatten() {
                if !webhook_matches(api, hook, a, &obj) {
                    continue;
                }
                let hook_name = util::s(hook, "/name").to_string();
                let fail_open = util::s(hook, "/failurePolicy") == "Ignore";
                match call(api, hook, a, &obj).await {
                    Ok(response) => {
                        if response.get("allowed").and_then(Value::as_bool) != Some(true) {
                            let status = response.get("status").cloned().unwrap_or_default();
                            let code = status.get("code").and_then(Value::as_u64).unwrap_or(403) as u16;
                            let message = status.get("message").and_then(Value::as_str).unwrap_or("denied");
                            return Err(ApiError::new(
                                StatusCode::from_u16(code).unwrap_or(StatusCode::FORBIDDEN),
                                "Forbidden",
                                format!("admission webhook \"{hook_name}\" denied the request: {message}"),
                            ));
                        }
                        if mutating {
                            if let Some(p) = response.get("patch").and_then(Value::as_str) {
                                let ops: Value = base64::engine::general_purpose::STANDARD
                                    .decode(p)
                                    .ok()
                                    .and_then(|b| serde_json::from_slice(&b).ok())
                                    .unwrap_or(Value::Null);
                                if ops.is_array() {
                                    patch::json_patch(&mut obj, &ops).map_err(|e| {
                                        ApiError::new(
                                            StatusCode::INTERNAL_SERVER_ERROR,
                                            "InternalError",
                                            format!("admission webhook \"{hook_name}\" returned an invalid patch: {e}"),
                                        )
                                    })?;
                                }
                            }
                        }
                    }
                    Err(_) if fail_open => {}
                    Err(e) => {
                        return Err(ApiError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "InternalError",
                            format!("Internal error occurred: failed calling webhook \"{hook_name}\": {e:#}"),
                        ));
                    }
                }
            }
        }
    }
    Ok(obj)
}

async fn call(api: &ApiState, hook: &Value, a: &Attributes<'_>, obj: &Value) -> Result<Value> {
    let uid = format!("{:032x}", rand_u128());
    let gvk = json!({"group": a.rt.group, "version": a.rt.version, "kind": a.rt.kind});
    let gvr = json!({"group": a.rt.group, "version": a.rt.version, "resource": a.rt.plural});
    let mut request = json!({
        "uid": uid,
        "kind": gvk,
        "resource": gvr,
        "requestKind": gvk,
        "requestResource": gvr,
        "name": a.name,
        "namespace": a.namespace,
        "operation": a.operation,
        "userInfo": {"username": "system:admin", "groups": ["system:masters", "system:authenticated"]},
        "object": if a.operation == "DELETE" { Value::Null } else { obj.clone() },
        "oldObject": a.old.cloned().unwrap_or(Value::Null),
        "dryRun": a.dry_run,
        "options": {"kind": if a.operation == "CREATE" { "CreateOptions" } else { "UpdateOptions" }, "apiVersion": "meta.k8s.io/v1"},
    });
    if let Some(sub) = a.sub {
        request["subResource"] = json!(sub);
    }
    let review = json!({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview", "request": request});
    let timeout = Duration::from_secs(hook.get("timeoutSeconds").and_then(Value::as_u64).unwrap_or(10).clamp(1, 30));
    let config = hook.get("clientConfig").cloned().unwrap_or_default();
    let ca = config
        .get("caBundle")
        .and_then(Value::as_str)
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
    let (addr, server_name, path) = match config.get("url").and_then(Value::as_str) {
        Some(url) => {
            let rest = url.strip_prefix("https://").context("webhook URL must be https")?;
            let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
            let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority).trim_matches(['[', ']']).to_string();
            let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:443") };
            (addr, host, path)
        }
        None => {
            let svc_ns = util::s(&config, "/service/namespace").to_string();
            let svc_name = util::s(&config, "/service/name").to_string();
            let port = config.pointer("/service/port").and_then(Value::as_i64).unwrap_or(443);
            let path = match util::s(&config, "/service/path") {
                "" => "/".to_string(),
                p => p.to_string(),
            };
            let addr = resolve_service(api, &svc_ns, &svc_name, port)?;
            (addr, format!("{svc_name}.{svc_ns}.svc"), path)
        }
    };
    let body = serde_json::to_vec(&review)?;
    let response = tokio::time::timeout(timeout, post(&addr, &server_name, &path, body, ca)).await.context("webhook timed out")??;
    response.get("response").cloned().context("webhook returned no response")
}

/// A ready backend of a Service port, as `ip:port`.
fn resolve_service(api: &ApiState, ns: &str, name: &str, port: i64) -> Result<String> {
    let svc = api.get_object("", "services", ns, name).with_context(|| format!("service \"{ns}/{name}\" not found"))?;
    let ports = svc.pointer("/spec/ports").and_then(Value::as_array).cloned().unwrap_or_default();
    let svc_port = ports
        .iter()
        .find(|p| p.get("port").and_then(Value::as_i64) == Some(port))
        .or_else(|| ports.first())
        .with_context(|| format!("service \"{ns}/{name}\" has no port {port}"))?;
    let ep = api.get_object("", "endpoints", ns, name).with_context(|| format!("no endpoints for service \"{ns}/{name}\""))?;
    let port_name = util::s(svc_port, "/name");
    for subset in ep.get("subsets").and_then(Value::as_array).into_iter().flatten() {
        let Some(ip) = subset.pointer("/addresses/0/ip").and_then(Value::as_str) else { continue };
        let ports = subset.get("ports").and_then(Value::as_array).cloned().unwrap_or_default();
        let target = ports
            .iter()
            .find(|p| util::s(p, "/name") == port_name)
            .or_else(|| ports.first())
            .and_then(|p| p.get("port").and_then(Value::as_i64));
        if let Some(target) = target {
            return Ok(format!("{ip}:{target}"));
        }
    }
    bail!("no ready endpoints for service \"{ns}/{name}\"")
}

async fn post(addr: &str, server_name: &str, path: &str, body: Vec<u8>, ca: Option<Vec<u8>>) -> Result<Value> {
    let tcp = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("failed to connect to {addr}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?;
    let config = match ca.filter(|c| !c.is_empty()) {
        Some(pem) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let _ = roots.add(cert?);
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        // Without a CA bundle there is nothing to verify against; Kubernetes
        // would use the system roots, which never sign in-cluster webhooks.
        None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth(),
    };
    let name = ServerName::try_from(server_name.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::post(path)
        .header(hyper::header::HOST, server_name)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::from(body)))?;
    let resp = sender.send_request(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        bail!("webhook answered {status}: {}", String::from_utf8_lossy(&bytes));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn rand_u128() -> u128 {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    u128::from_le_bytes(b)
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> ResourceType {
        ResourceType {
            group: "cert-manager.io".into(),
            version: "v1".into(),
            kind: "CertificateRequest".into(),
            plural: "certificaterequests".into(),
            singular: "certificaterequest".into(),
            namespaced: true,
            short_names: vec![],
            categories: vec![],
            has_status: true,
            has_scale: false,
            printer_columns: vec![],
            custom: true,
        }
    }

    #[test]
    fn rules_match_groups_resources_and_scope() {
        let rt = rt();
        let a = Attributes { rt: &rt, operation: "CREATE", namespace: "x", name: "n", sub: None, old: None, dry_run: false };
        let rule = json!({"operations": ["CREATE", "UPDATE"], "apiGroups": ["cert-manager.io"], "apiVersions": ["v1"], "resources": ["*/*"]});
        assert!(rule_matches(&rule, &a));
        let rule = json!({"operations": ["DELETE"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]});
        assert!(!rule_matches(&rule, &a));
        let rule = json!({"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["certificaterequests"], "scope": "Cluster"});
        assert!(!rule_matches(&rule, &a));
        let status = Attributes { sub: Some("status"), ..a };
        let rule = json!({"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["certificaterequests"]});
        assert!(!rule_matches(&rule, &status));
        let rule = json!({"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["certificaterequests/status"]});
        assert!(rule_matches(&rule, &status));
    }
}
