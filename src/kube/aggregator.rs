//! API aggregation, as kube-aggregator does it: an APIService with a
//! `spec.service` hands its group version to an extension API server running
//! in the cluster. A loop checks each one like the aggregator's availability
//! controller (the `Available` condition) and lists the resources it serves
//! in discovery; requests under `/apis/<group>/<version>/` are proxied to the
//! Service over TLS.
//!
//! The request is authorized here first, as any other. It reaches the
//! extension server without user identity (no front-proxy client
//! certificate), so the extension server sees it as anonymous: under RBAC
//! its delegated SubjectAccessReviews for `system:anonymous` are denied
//! unless a binding allows that user.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, HOST, TRANSFER_ENCODING, UPGRADE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use super::admission;
use super::controllers::util;
use super::registry::ResourceType;
use super::server::{ApiError, ApiState, Body, Req};
use crate::obs::resource_metrics;

const GROUP: &str = "apiregistration.k8s.io";
const PLURAL: &str = "apiservices";
/// Upper bound for one APIService's availability check.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The APIService that delegates `group/version` to a Service, if any.
pub(crate) fn service_for(state: &ApiState, group: &str, version: &str) -> Option<Value> {
    if group.is_empty() {
        return None;
    }
    let api = state.get_object(GROUP, PLURAL, "", &format!("{version}.{group}"))?;
    api.pointer("/spec/service/name").and_then(Value::as_str).is_some_and(|n| !n.is_empty()).then_some(api)
}

/// Send one request to the extension server behind `api`.
async fn send(state: &ApiState, api: &Value, request: Request<Full<Bytes>>) -> Result<Response<Incoming>> {
    let ns = util::s(api, "/spec/service/namespace");
    let name = util::s(api, "/spec/service/name");
    let port = api.pointer("/spec/service/port").and_then(Value::as_i64).unwrap_or(443);
    let backend = admission::resolve_service(state, ns, name, port)?;
    let ca = api
        .pointer("/spec/caBundle")
        .and_then(Value::as_str)
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
    let insecure = api.pointer("/spec/insecureSkipTLSVerify").and_then(Value::as_bool) == Some(true);
    let server_name = format!("{name}.{ns}.svc");
    let io = admission::tls_dial(state, &backend, &server_name, ca, insecure).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut request = request;
    request.headers_mut().insert(HOST, server_name.parse()?);
    Ok(sender.send_request(request).await?)
}

/// Proxy a request for an aggregated group version. The response streams,
/// so watches work.
pub(crate) async fn proxy(state: &Arc<ApiState>, api: &Value, path: &str, req: &Req) -> Result<Response<Body>, ApiError> {
    let unavailable = |e: anyhow::Error| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", format!("the server is currently unable to handle the request: {e:#}"))
    };
    // Like the aggregator: nothing reaches the extension server before the
    // availability check passed (which also put it in discovery).
    let available = util::conditions_of(api).iter().any(|c| util::s(c, "/type") == "Available" && util::s(c, "/status") == "True");
    if !available {
        return Err(unavailable(anyhow::anyhow!("service unavailable")));
    }
    let uri = if req.raw_query().is_empty() { path.to_string() } else { format!("{path}?{}", req.raw_query()) };
    let mut builder = Request::builder().method(req.method().clone()).uri(uri);
    for (k, v) in req.headers() {
        if k == HOST || k == CONNECTION || k == UPGRADE || k == TRANSFER_ENCODING || k.as_str() == "authorization" {
            continue;
        }
        builder = builder.header(k, v);
    }
    let request = builder.body(Full::new(req.body().clone())).map_err(|e| unavailable(e.into()))?;
    let response = tokio::time::timeout(Duration::from_secs(30), send(state, api, request))
        .await
        .map_err(|_| unavailable(anyhow::anyhow!("timed out reaching the extension API server")))?
        .map_err(unavailable)?;
    let (parts, body) = response.into_parts();
    let mut resp = Response::new(body.map_err(std::io::Error::other).boxed_unsync());
    *resp.status_mut() = parts.status;
    for (k, v) in &parts.headers {
        if k == CONNECTION || k == TRANSFER_ENCODING || k.as_str() == "keep-alive" {
            continue;
        }
        resp.headers_mut().append(k, v.clone());
    }
    Ok(resp)
}

/// The resources an extension server lists for its group version.
async fn discover(state: &ApiState, api: &Value, group: &str, version: &str) -> Result<Vec<ResourceType>> {
    let request = Request::get(format!("/apis/{group}/{version}"))
        .header(hyper::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::new()))?;
    // One deadline for the whole exchange, body included: a backend that
    // sends headers and then stalls must not hold the availability loop.
    let (status, bytes) = tokio::time::timeout(Duration::from_secs(5), async {
        let response = send(state, api, request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        anyhow::Ok((status, bytes))
    })
    .await
    .context("timed out")??;
    if !status.is_success() {
        bail!("failing or missing response from https://{}/apis/{group}/{version}: bad status from {status}", util::s(api, "/spec/service/name"));
    }
    Ok(types_of(&serde_json::from_slice(&bytes)?, group, version))
}

/// Resource types from an APIResourceList (subresources become flags).
fn types_of(list: &Value, group: &str, version: &str) -> Vec<ResourceType> {
    let all: Vec<Value> = list.get("resources").and_then(Value::as_array).cloned().unwrap_or_default();
    let subresource = |name: &str, sub: &str| all.iter().any(|r| util::s(r, "/name") == format!("{name}/{sub}"));
    let strings = |r: &Value, key: &str| -> Vec<String> {
        r.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
    };
    all.iter()
        .filter(|r| !util::s(r, "/name").contains('/'))
        .map(|r| {
            let plural = util::s(r, "/name").to_string();
            let kind = util::s(r, "/kind").to_string();
            ResourceType {
                group: group.into(),
                version: version.into(),
                singular: match util::s(r, "/singularName") {
                    "" => kind.to_lowercase(),
                    s => s.to_string(),
                },
                namespaced: r.get("namespaced").and_then(Value::as_bool).unwrap_or(false),
                short_names: strings(r, "shortNames"),
                categories: strings(r, "categories"),
                has_status: subresource(&plural, "status"),
                has_scale: subresource(&plural, "scale"),
                printer_columns: Vec::new(),
                custom: false,
                aggregated: true,
                kind,
                plural,
            }
        })
        .collect()
}

/// Set the `Available` condition (kept first, as the aggregator does).
fn set_available(state: &ApiState, api: &Value, ok: bool, reason: &str, message: &str) {
    // Every server discovers aggregated APIs; the leader writes status.
    if !state.leads() {
        return;
    }
    let mut conditions = util::conditions_of(api);
    let status = if ok { "True" } else { "False" };
    let now = util::rfc3339(util::now());
    match conditions.iter_mut().find(|c| util::s(c, "/type") == "Available") {
        Some(c) => {
            if util::s(c, "/status") != status {
                c["lastTransitionTime"] = json!(now);
            }
            c["status"] = json!(status);
            c["reason"] = json!(reason);
            c["message"] = json!(message);
        }
        None => conditions.insert(0, json!({"type": "Available", "status": status, "lastTransitionTime": now, "reason": reason, "message": message})),
    }
    let mut status = api.get("status").cloned().unwrap_or_else(|| json!({}));
    status["conditions"] = json!(conditions);
    let _ = util::write_status(state, GROUP, PLURAL, api, status);
}

async fn check(state: &ApiState, api: &Value) -> Option<(String, String)> {
    let group = util::s(api, "/spec/group").to_string();
    let version = util::s(api, "/spec/version").to_string();
    if service_for(state, &group, &version).is_none() {
        set_available(state, api, true, "Local", "Local APIServices are always available");
        return None;
    }
    let ns = util::s(api, "/spec/service/namespace");
    let name = util::s(api, "/spec/service/name");
    let (ok, reason, message) = if state.get_object("", "services", ns, name).is_none() {
        state.registry.set_aggregated(&group, &version, Vec::new());
        (false, "ServiceNotFound", format!("service/{name} in \"{ns}\" is not present"))
    } else {
        match discover(state, api, &group, &version).await {
            Ok(types) => {
                state.registry.set_aggregated(&group, &version, types);
                (true, "Passed", "all checks passed".to_string())
            }
            Err(e) => {
                state.registry.set_aggregated(&group, &version, Vec::new());
                let reason = if format!("{e:#}").contains("endpoints") { "MissingEndpoints" } else { "FailedDiscoveryCheck" };
                (false, reason, format!("{e:#}"))
            }
        }
    };
    // Re-read: the request may have taken a while.
    if let Some(current) = state.get_object(GROUP, PLURAL, "", &util::name(api)) {
        set_available(state, &current, ok, reason, &message);
    }
    Some((group, version))
}

/// What the API server serves itself for a group version no APIService
/// delegates: the resource metrics API from the observability store.
fn local_types(state: &ApiState, group: &str, version: &str) -> Vec<ResourceType> {
    if group == resource_metrics::GROUP && version == resource_metrics::VERSION && state.obs.get().is_some() {
        resource_metrics::types()
    } else {
        Vec::new()
    }
}

/// The availability loop.
pub async fn run(state: Arc<ApiState>) {
    let mut served: HashSet<(String, String)> = HashSet::new();
    loop {
        let (apis, _) = state.store().list(GROUP, PLURAL, None);
        let mut now = HashSet::new();
        for api in &apis {
            // Belt and braces: whatever a check waits on, one slow APIService
            // must never stop the others from being checked.
            match tokio::time::timeout(CHECK_TIMEOUT, check(&state, api)).await {
                Ok(Some(gv)) => {
                    now.insert(gv);
                }
                Ok(None) => {}
                Err(_) => {
                    let gv = (util::s(api, "/spec/group").to_string(), util::s(api, "/spec/version").to_string());
                    state.registry.set_aggregated(&gv.0, &gv.1, Vec::new());
                    if let Some(current) = state.get_object(GROUP, PLURAL, "", &util::name(api)) {
                        set_available(&state, &current, false, "FailedDiscoveryCheck", "availability check timed out");
                    }
                    now.insert(gv);
                }
            }
        }
        for (group, version) in served.difference(&now) {
            state.registry.set_aggregated(group, version, local_types(&state, group, version));
        }
        served = now;
        tokio::time::sleep(Duration::from_secs(if served.is_empty() { 2 } else { 1 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_list_to_types() {
        let list = json!({"kind": "APIResourceList", "groupVersion": "wardle.example.com/v1alpha1", "resources": [
            {"name": "flunders", "singularName": "flunder", "namespaced": true, "kind": "Flunder", "verbs": ["get", "list"]},
            {"name": "flunders/status", "namespaced": true, "kind": "Flunder", "verbs": ["get"]},
            {"name": "fischers", "singularName": "", "namespaced": false, "kind": "Fischer", "shortNames": ["fi"]},
        ]});
        let types = types_of(&list, "wardle.example.com", "v1alpha1");
        assert_eq!(types.len(), 2);
        assert!(types.iter().all(|t| t.aggregated && t.group == "wardle.example.com" && t.version == "v1alpha1"));
        assert!(types[0].namespaced && types[0].has_status && types[0].singular == "flunder");
        assert!(!types[1].namespaced && !types[1].has_status && types[1].singular == "fischer" && types[1].short_names == ["fi"]);
    }
}
