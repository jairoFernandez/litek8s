//! Dynamic admission: calling the webhooks registered through
//! MutatingWebhookConfiguration and ValidatingWebhookConfiguration objects
//! (cert-manager, kyverno, ... rely on them).
//!
//! Supported: rules (operations, groups, versions, resources/subresources,
//! scope), namespace and object selectors, `failurePolicy`, `timeoutSeconds`,
//! service and URL client configs, JSONPatch responses. `matchConditions`
//! (CEL) are not evaluated: a webhook whose rules match is called.
//! Requests made by the in-process controllers skip webhooks for now.
//!
//! A Service-backed webhook is called at a ready endpoint's pod address.
//! When the API server cannot reach that address (a pod on another node, or
//! Docker in a VM), the connection is tunneled through the node agent that
//! runs the pod, the way `kubectl port-forward` is.

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

use super::agent::hub::StreamEvent;
use super::cel;
use super::agent::protocol::Message;
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

/// The CEL variables of an admission request.
fn cel_env(a: &Attributes<'_>, obj: &Value) -> cel::Env {
    let mut env = cel::Env::default();
    env.vars.insert("object".into(), if a.operation == "DELETE" { cel::Val::Null } else { cel::Val::from_json(obj) });
    env.vars.insert("oldObject".into(), a.old.map(cel::Val::from_json).unwrap_or(cel::Val::Null));
    env.vars.insert(
        "request".into(),
        cel::Val::from_json(&json!({
            "operation": a.operation,
            "namespace": a.namespace,
            "name": a.name,
            "kind": {"group": a.rt.group, "version": a.rt.version, "kind": a.rt.kind},
            "resource": {"group": a.rt.group, "version": a.rt.version, "resource": a.rt.plural},
            "subResource": a.sub.unwrap_or_default(),
            "userInfo": {"username": "system:admin", "groups": ["system:masters", "system:authenticated"]},
            "dryRun": a.dry_run,
        })),
    );
    env
}

fn webhook_matches(api: &ApiState, hook: &Value, a: &Attributes, obj: &Value) -> bool {
    let rules = hook.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
    if !rules.iter().any(|r| rule_matches(r, a)) {
        return false;
    }
    // matchConditions: every one must be true (an error still calls).
    let conds = hook.get("matchConditions").and_then(Value::as_array).filter(|c| !c.is_empty());
    if let Some(conds) = conds {
        let env = cel_env(a, obj);
        for c in conds {
            let r = cel::compile(util::s(c, "/expression")).and_then(|p| p.eval(&env));
            if r == Ok(cel::Val::Bool(false)) {
                return false;
            }
        }
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
                            // The API server's wording (webhookerrors.ToStatusErr).
                            let status = response.get("status").cloned().unwrap_or_default();
                            let code = status.get("code").and_then(Value::as_u64).filter(|c| *c >= 400).unwrap_or(400) as u16;
                            let text = |k: &str| status.get(k).and_then(Value::as_str).filter(|m| !m.is_empty());
                            let message = match (text("message"), text("reason")) {
                                (Some(m), _) | (None, Some(m)) => format!("admission webhook \"{hook_name}\" denied the request: {m}"),
                                (None, None) => format!("admission webhook \"{hook_name}\" denied the request without explanation"),
                            };
                            let code = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST);
                            let reason = match code {
                                StatusCode::FORBIDDEN => "Forbidden",
                                StatusCode::UNPROCESSABLE_ENTITY => "Invalid",
                                StatusCode::NOT_FOUND => "NotFound",
                                StatusCode::CONFLICT => "Conflict",
                                _ => "BadRequest",
                            };
                            return Err(ApiError::new(code, reason, message));
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
    validating_policies(api, a, &obj)?;
    Ok(obj)
}

/// ValidatingAdmissionPolicies: every binding whose policy matches the
/// request evaluates the policy's CEL validations; a failed one with the
/// `Deny` action rejects the request.
fn validating_policies(api: &ApiState, a: &Attributes<'_>, obj: &Value) -> Result<(), ApiError> {
    const GROUP: &str = "admissionregistration.k8s.io";
    let (bindings, _) = api.store().list(GROUP, "validatingadmissionpolicybindings", None);
    if bindings.is_empty() {
        return Ok(());
    }
    let ns_obj = if a.rt.namespaced { api.get_object("", "namespaces", "", a.namespace) } else { None };
    let matches = |constraints: Option<&Value>| -> bool {
        let Some(c) = constraints.filter(|c| c.is_object()) else { return true };
        let as_hook = json!({
            "rules": c.get("resourceRules").cloned().unwrap_or(json!([{"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]}])),
            "namespaceSelector": c.get("namespaceSelector").cloned().unwrap_or(Value::Null),
            "objectSelector": c.get("objectSelector").cloned().unwrap_or(Value::Null),
        });
        if !webhook_matches(api, &as_hook, a, obj) {
            return false;
        }
        let excluded = c.get("excludeResourceRules").and_then(Value::as_array).is_some_and(|rules| rules.iter().any(|r| rule_matches(r, a)));
        !excluded
    };
    for binding in &bindings {
        let policy_name = util::s(binding, "/spec/policyName");
        let Some(policy) = api.get_object(GROUP, "validatingadmissionpolicies", "", policy_name) else { continue };
        if !matches(policy.pointer("/spec/matchConstraints")) || !matches(binding.pointer("/spec/matchResources")) {
            continue;
        }
        let actions = strings(binding.pointer("/spec/validationActions"));
        let deny = actions.is_empty() || actions.contains(&"Deny");
        let fail_open = util::s(&policy, "/spec/failurePolicy") == "Ignore";
        let reject = |message: String, reason: &str| -> ApiError {
            let message = format!(
                "ValidatingAdmissionPolicy '{policy_name}' with binding '{}' denied request: {message}",
                util::name(binding)
            );
            match reason {
                "Forbidden" => ApiError::new(StatusCode::FORBIDDEN, "Forbidden", message),
                "Unauthorized" => ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized", message),
                "RequestEntityTooLarge" => ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "RequestEntityTooLarge", message),
                _ => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", message),
            }
        };
        let mut env = cel::Env::default();
        let val = |v: Option<&Value>| v.map(cel::Val::from_json).unwrap_or(cel::Val::Null);
        env.vars.insert("object".into(), if a.operation == "DELETE" { cel::Val::Null } else { cel::Val::from_json(obj) });
        env.vars.insert("oldObject".into(), val(a.old));
        env.vars.insert("namespaceObject".into(), val(ns_obj.as_ref()));
        env.vars.insert(
            "request".into(),
            cel::Val::from_json(&json!({
                "operation": a.operation,
                "namespace": a.namespace,
                "name": a.name,
                "kind": {"group": a.rt.group, "version": a.rt.version, "kind": a.rt.kind},
                "resource": {"group": a.rt.group, "version": a.rt.version, "resource": a.rt.plural},
                "subResource": a.sub.unwrap_or_default(),
                "userInfo": {"username": "system:admin", "groups": ["system:masters", "system:authenticated"]},
                "dryRun": a.dry_run,
            })),
        );
        // Parameters: the object `paramRef` names, of kind `paramKind`.
        let mut params = cel::Val::Null;
        if let Some(kind) = policy.pointer("/spec/paramKind").filter(|k| k.is_object()) {
            let (group, version) = util::s(kind, "/apiVersion").rsplit_once('/').map_or(("", util::s(kind, "/apiVersion")), |(g, v)| (g, v));
            let _ = version;
            let rt = api.resource_for_kind(group, util::s(kind, "/kind"));
            let pref = binding.pointer("/spec/paramRef");
            let pname = pref.map(|p| util::s(p, "/name")).unwrap_or_default();
            let pns = match pref.map(|p| util::s(p, "/namespace")).unwrap_or_default() {
                "" if rt.as_ref().is_some_and(|r| r.namespaced) => a.namespace,
                other => other,
            };
            let found = rt.and_then(|rt| api.get_object(&rt.group, &rt.plural, if rt.namespaced { pns } else { "" }, pname));
            match found {
                Some(p) => params = cel::Val::from_json(&p),
                None if pref.is_some_and(|p| util::s(p, "/parameterNotFoundAction") == "Allow") => continue,
                None => {
                    if deny {
                        return Err(reject(format!("failed to configure binding: no params found for policy binding with `Deny` parameterNotFoundAction"), "Invalid"));
                    }
                    continue;
                }
            }
        }
        env.vars.insert("params".into(), params);
        let eval = |src: &str, env: &cel::Env| cel::compile(src).and_then(|p| p.eval(env));
        let mut failure = None;
        let mut variables = std::collections::BTreeMap::new();
        for var in policy.pointer("/spec/variables").and_then(Value::as_array).into_iter().flatten() {
            env.vars.insert("variables".into(), cel::Val::Map(variables.clone()));
            match eval(util::s(var, "/expression"), &env) {
                Ok(v) => {
                    variables.insert(util::s(var, "/name").to_string(), v);
                }
                Err(e) => {
                    failure.get_or_insert(format!("variable '{}' resulted in error: {e}", util::s(var, "/name")));
                }
            }
        }
        env.vars.insert("variables".into(), cel::Val::Map(variables));
        // matchConditions: all must be true for the policy to apply.
        let mut applies = true;
        for cond in policy.pointer("/spec/matchConditions").and_then(Value::as_array).into_iter().flatten() {
            match eval(util::s(cond, "/expression"), &env) {
                Ok(cel::Val::Bool(true)) => {}
                Ok(_) => applies = false,
                Err(e) => {
                    failure.get_or_insert(format!("matchCondition '{}' resulted in error: {e}", util::s(cond, "/name")));
                }
            }
        }
        if !applies {
            continue;
        }
        for v in policy.pointer("/spec/validations").and_then(Value::as_array).into_iter().flatten() {
            let expr = util::s(v, "/expression");
            let outcome = match eval(expr, &env) {
                Ok(cel::Val::Bool(true)) => continue,
                Ok(cel::Val::Bool(false)) => Ok(()),
                Ok(other) => Err(format!("expression '{expr}' resulted in a {} instead of a bool", other)),
                Err(e) => Err(format!("expression '{expr}' resulted in error: {e}")),
            };
            match outcome {
                Ok(()) => {
                    let message = match eval(util::s(v, "/messageExpression"), &env) {
                        Ok(cel::Val::Str(m)) if !m.trim().is_empty() && !util::s(v, "/messageExpression").is_empty() => m,
                        _ => match util::s(v, "/message") {
                            "" => format!("failed expression: {expr}"),
                            m => m.to_string(),
                        },
                    };
                    if deny {
                        return Err(reject(message, util::s(v, "/reason")));
                    }
                }
                Err(e) => {
                    failure.get_or_insert(e);
                }
            }
        }
        if let Some(e) = failure {
            if deny && !fail_open {
                return Err(reject(e, "Invalid"));
            }
        }
    }
    Ok(())
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
        "options": {"kind": match a.operation { "CREATE" => "CreateOptions", "DELETE" => "DeleteOptions", _ => "UpdateOptions" }, "apiVersion": "meta.k8s.io/v1"},
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
    let (backend, server_name, path) = match config.get("url").and_then(Value::as_str) {
        Some(url) => {
            let (addr, host, path) = url_parts(url)?;
            (Backend { addr, ..Default::default() }, host, path)
        }
        None => {
            let svc_ns = util::s(&config, "/service/namespace").to_string();
            let svc_name = util::s(&config, "/service/name").to_string();
            let port = config.pointer("/service/port").and_then(Value::as_i64).unwrap_or(443);
            let path = match util::s(&config, "/service/path") {
                "" => "/".to_string(),
                p => p.to_string(),
            };
            let backend = resolve_service(api, &svc_ns, &svc_name, port)?;
            (backend, format!("{svc_name}.{svc_ns}.svc"), path)
        }
    };
    // Like the API server, tell the webhook how long it has.
    let path = format!("{path}{}timeout={}s", if path.contains('?') { '&' } else { '?' }, timeout.as_secs().max(1));
    let url = format!("https://{server_name}:{}{path}", backend.addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("443"));
    let body = serde_json::to_vec(&review)?;
    let response = match tokio::time::timeout(timeout, post(api, &backend, &server_name, &path, body, ca)).await {
        Ok(r) => r.with_context(|| format!("failed to call webhook: Post \"{url}\""))?,
        Err(_) => bail!("failed to call webhook: Post \"{url}\": context deadline exceeded"),
    };
    response.get("response").cloned().context("webhook returned no response")
}

/// Convert custom resources to `desired` (`group/version`) as their CRD's
/// `spec.conversion` says: `None` only relabels them, `Webhook` asks the
/// conversion webhook (a ConversionReview).
pub(crate) async fn convert_objects(api: &ApiState, crd: &Value, objects: Vec<Value>, desired: &str) -> Result<Vec<Value>, ApiError> {
    let pending: Vec<usize> = (0..objects.len()).filter(|i| util::s(&objects[*i], "/apiVersion") != desired).collect();
    if pending.is_empty() {
        return Ok(objects);
    }
    let mut objects = objects;
    if util::s(crd, "/spec/conversion/strategy") != "Webhook" {
        for i in pending {
            objects[i]["apiVersion"] = json!(desired);
        }
        return Ok(objects);
    }
    let failed = |e: anyhow::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", format!("conversion webhook for {} failed: {e:#}", util::name(crd)));
    let config = crd.pointer("/spec/conversion/webhook/clientConfig").cloned().unwrap_or_default();
    let ca = config.get("caBundle").and_then(Value::as_str).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
    let (backend, server_name, path) = match config.get("url").and_then(Value::as_str) {
        Some(url) => {
            let (addr, host, path) = url_parts(url).map_err(failed)?;
            (Backend { addr, ..Default::default() }, host, path)
        }
        None => {
            let ns = util::s(&config, "/service/namespace").to_string();
            let name = util::s(&config, "/service/name").to_string();
            let port = config.pointer("/service/port").and_then(Value::as_i64).unwrap_or(443);
            let path = match util::s(&config, "/service/path") {
                "" => "/".to_string(),
                p => p.to_string(),
            };
            (resolve_service(api, &ns, &name, port).map_err(failed)?, format!("{name}.{ns}.svc"), path)
        }
    };
    let review_versions = crd.pointer("/spec/conversion/webhook/conversionReviewVersions").and_then(Value::as_array).cloned().unwrap_or_default();
    let version = if review_versions.iter().any(|v| v == "v1") { "apiextensions.k8s.io/v1" } else { "apiextensions.k8s.io/v1beta1" };
    let review = json!({
        "apiVersion": version,
        "kind": "ConversionReview",
        "request": {
            "uid": format!("{:032x}", rand_u128()),
            "desiredAPIVersion": desired,
            "objects": pending.iter().map(|i| objects[*i].clone()).collect::<Vec<_>>(),
        },
    });
    let body = serde_json::to_vec(&review).map_err(|e| failed(e.into()))?;
    let response = tokio::time::timeout(Duration::from_secs(30), post(api, &backend, &server_name, &path, body, ca))
        .await
        .map_err(|_| failed(anyhow::anyhow!("timed out")))?
        .map_err(failed)?;
    let result = response.pointer("/response/result/status").and_then(Value::as_str).unwrap_or("Success");
    if result != "Success" {
        let msg = response.pointer("/response/result/message").and_then(Value::as_str).unwrap_or("conversion failed");
        return Err(failed(anyhow::anyhow!("{msg}")));
    }
    let converted = response.pointer("/response/convertedObjects").and_then(Value::as_array).cloned().unwrap_or_default();
    if converted.len() != pending.len() {
        return Err(failed(anyhow::anyhow!("returned {} objects for {}", converted.len(), pending.len())));
    }
    for (i, obj) in pending.into_iter().zip(converted) {
        // The webhook may not change identity or bookkeeping metadata.
        let meta = objects[i]["metadata"].clone();
        let mut obj = obj;
        obj["metadata"] = meta;
        for k in ["labels", "annotations"] {
            if let Some(v) = objects[i].pointer(&format!("/metadata/{k}")) {
                obj["metadata"][k] = v.clone();
            }
        }
        objects[i] = obj;
    }
    Ok(objects)
}

/// Where a webhook listens: `ip:port`, and for a Service endpoint the pod
/// behind it and its node.
#[derive(Default)]
pub(crate) struct Backend {
    addr: String,
    node: String,
    pod_uid: String,
    port: u16,
}

/// A ready backend of a Service port.
pub(crate) fn resolve_service(api: &ApiState, ns: &str, name: &str, port: i64) -> Result<Backend> {
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
            return Ok(Backend {
                addr: format!("{ip}:{target}"),
                node: util::s(subset, "/addresses/0/nodeName").to_string(),
                pod_uid: util::s(subset, "/addresses/0/targetRef/uid").to_string(),
                port: target as u16,
            });
        }
    }
    bail!("no ready endpoints for service \"{ns}/{name}\"")
}

pub(crate) trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Addresses the API server could not reach directly; tunneled from then on.
static UNREACHABLE: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);

async fn dial(api: &ApiState, backend: &Backend) -> Result<Box<dyn Io>> {
    let addr = &backend.addr;
    let tunnel = !backend.pod_uid.is_empty() && (api.hub.is_connected(&backend.node) || api.node_server_addr(&backend.node).is_some());
    if !tunnel || !UNREACHABLE.lock().unwrap().as_ref().is_some_and(|s| s.contains(addr)) {
        match tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr.as_str())).await {
            Ok(Ok(tcp)) => return Ok(Box::new(tcp)),
            Ok(Err(e)) if !tunnel || e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(e).with_context(|| format!("failed to connect to {addr}"));
            }
            Err(_) if !tunnel => bail!("timed out connecting to {addr}"),
            _ => {
                UNREACHABLE.lock().unwrap().get_or_insert_default().insert(addr.clone());
            }
        }
    }
    pod_stream(api, &backend.node, &backend.pod_uid, backend.port).await
}

/// A byte stream to `port` of a pod, through its node's agent connection:
/// on this server, or (HA) through the server the node is connected to.
pub(crate) async fn pod_stream(api: &ApiState, node: &str, pod_uid: &str, port: u16) -> Result<Box<dyn Io>> {
    if !api.hub.is_connected(node) {
        if let (Some(cluster), Some(addr)) = (api.ha(), api.node_server_addr(node)) {
            return Ok(Box::new(super::ha::tunnel(&cluster, &addr, node, pod_uid, port).await?));
        }
    }
    let pod_uid = pod_uid.to_string();
    let mut handle = api.hub.open_stream(node, |stream| Message::PortForward { stream, pod_uid, port }).await?;
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (mut from_us, mut to_us) = tokio::io::split(theirs);
    let input = handle.input();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 32 * 1024];
        while let Ok(n) = from_us.read(&mut buf).await {
            if n == 0 || !input.send(0, Bytes::copy_from_slice(&buf[..n])).await {
                break;
            }
        }
        input.close().await;
    });
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(event) = handle.events.recv().await {
            match event {
                StreamEvent::Data { channel: 2, .. } => {}
                StreamEvent::Data { bytes, .. } => {
                    if to_us.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                StreamEvent::Done { .. } => break,
            }
        }
        let _ = to_us.shutdown().await;
    });
    Ok(Box::new(ours))
}

/// A TLS connection to `backend` that verifies `server_name` against the
/// CA bundle (the system roots without one), or nothing when `insecure`.
pub(crate) async fn tls_dial(api: &ApiState, backend: &Backend, server_name: &str, ca: Option<Vec<u8>>, insecure: bool) -> Result<Box<dyn Io>> {
    let tcp = dial(api, backend).await?;
    if insecure {
        return Ok(Box::new(super::agent::probe::tls_connect_any(tcp, server_name).await?));
    }
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
        // Without a CA bundle the system roots apply, as in Kubernetes: an
        // in-cluster webhook's own certificate then fails verification.
        None => {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    let name = ServerName::try_from(server_name.to_string())?;
    Ok(Box::new(tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?))
}

/// `https://host[:port]/path` as (dial address, TLS server name, path).
fn url_parts(url: &str) -> Result<(String, String, String)> {
    let rest = url.strip_prefix("https://").context("webhook URL must be https")?;
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority).trim_matches(['[', ']']).to_string();
    let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:443") };
    Ok((addr, host, path))
}

async fn post(api: &ApiState, backend: &Backend, server_name: &str, path: &str, body: Vec<u8>, ca: Option<Vec<u8>>) -> Result<Value> {
    let tls = tls_dial(api, backend, server_name, ca, false).await?;
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
            aggregated: false,
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

    use std::sync::Mutex;

    use super::super::store::ObjectKey;

    fn api() -> Arc<ApiState> {
        Arc::new(ApiState::open(None, "127.0.0.1:0".into(), false).unwrap())
    }

    fn put(api: &ApiState, group: &str, plural: &str, ns: &str, obj: Value) {
        api.store().create(ObjectKey::new(group, plural, ns, util::s(&obj, "/metadata/name")), obj).unwrap();
    }

    fn attrs<'a>(rt: &'a ResourceType, operation: &'static str) -> Attributes<'a> {
        Attributes { rt, operation, namespace: "default", name: "p", sub: None, old: None, dry_run: false }
    }

    fn pod() -> Value {
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p", "namespace": "default", "labels": {"app": "a"}}, "spec": {"containers": [{"name": "c", "image": "nginx"}]}})
    }

    const ALL_PODS: &str = r#"[{"operations": ["CREATE", "UPDATE", "DELETE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods", "pods/status"]}]"#;

    type Handler = Box<dyn Fn(&str, &Value) -> (u16, Value) + Send + Sync>;

    struct Server {
        port: u16,
        ca: String,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
    }

    /// An HTTPS server for `localhost` and `hook.default.svc` that answers
    /// every POST with `handler(path and query, body)` after `delay`.
    async fn server(delay: Duration, handler: Handler) -> Server {
        server_on("127.0.0.1:0", delay, handler).await.unwrap()
    }

    /// None when `bind` is not available (no IPv6 loopback).
    async fn server_on(bind: &str, delay: Duration, handler: Handler) -> Option<Server> {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["localhost".to_string(), "hook.default.svc".to_string(), "::1".to_string()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind(bind).await.ok()?;
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let recorded = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let (acceptor, handler, seen) = (acceptor.clone(), handler.clone(), recorded.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (handler, seen) = (handler.clone(), seen.clone());
                        async move {
                            let path = req.uri().to_string();
                            let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                            let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                            seen.lock().unwrap().push((path.clone(), body.clone()));
                            tokio::time::sleep(delay).await;
                            let (status, reply) = handler(&path, &body);
                            let bytes = Bytes::from(serde_json::to_vec(&reply).unwrap());
                            Ok::<_, std::convert::Infallible>(hyper::Response::builder().status(status).body(Full::new(bytes)).unwrap())
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await;
                });
            }
        });
        Some(Server { port, ca: base64::engine::general_purpose::STANDARD.encode(ca.pem()), seen })
    }

    fn review(request: &Value, response: Value) -> (u16, Value) {
        let mut response = response;
        response["uid"] = request.pointer("/request/uid").cloned().unwrap_or_default();
        (200, json!({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview", "response": response}))
    }

    fn b64_patch(ops: Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&ops).unwrap())
    }

    fn webhook_config(api: &ApiState, mutating: bool, name: &str, hooks: Value) {
        let (plural, kind) = if mutating {
            ("mutatingwebhookconfigurations", "MutatingWebhookConfiguration")
        } else {
            ("validatingwebhookconfigurations", "ValidatingWebhookConfiguration")
        };
        put(api, "admissionregistration.k8s.io", plural, "", json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": kind, "metadata": {"name": name}, "webhooks": hooks}));
    }

    fn url_hook(name: &str, s: &Server, path: &str, extra: Value) -> Value {
        let mut hook = json!({
            "name": name,
            "clientConfig": {"url": format!("https://localhost:{}{path}", s.port), "caBundle": s.ca},
            "rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(),
            "sideEffects": "None",
            "admissionReviewVersions": ["v1"],
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            hook[k] = v.clone();
        }
        hook
    }

    #[test]
    fn rules_match_wildcards_subresources_versions_and_scope() {
        let rt = rt();
        let a = Attributes { rt: &rt, operation: "UPDATE", namespace: "x", name: "n", sub: None, old: None, dry_run: false };
        let rule = |resources: Value| json!({"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": resources});
        assert!(rule_matches(&rule(json!(["*"])), &a));
        assert!(!rule_matches(&rule(json!(["certificaterequests/*"])), &a));
        assert!(!rule_matches(&json!({"operations": ["UPDATE"], "apiGroups": ["cert-manager.io"], "apiVersions": ["v1alpha1"], "resources": ["*"]}), &a));
        assert!(!rule_matches(&json!({"operations": ["UPDATE"], "apiGroups": [""], "apiVersions": ["*"], "resources": ["*"]}), &a));
        assert!(!rule_matches(&json!({"apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]}), &a));
        assert!(rule_matches(&json!({"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"], "scope": "Namespaced"}), &a));

        let status = Attributes { sub: Some("status"), ..a };
        assert!(!rule_matches(&rule(json!(["*"])), &status));
        assert!(rule_matches(&rule(json!(["certificaterequests/*"])), &status));
        assert!(rule_matches(&rule(json!(["*/status"])), &status));
        assert!(!rule_matches(&rule(json!(["*/scale"])), &status));
        assert!(!rule_matches(&rule(json!(["orders/*"])), &status));

        let cluster = ResourceType { namespaced: false, ..rt.clone() };
        let c = Attributes { rt: &cluster, operation: "CREATE", namespace: "", name: "n", sub: None, old: None, dry_run: false };
        assert!(rule_matches(&json!({"operations": ["CREATE"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"], "scope": "Cluster"}), &c));
        assert!(!rule_matches(&json!({"operations": ["CREATE"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"], "scope": "Namespaced"}), &c));
    }

    #[test]
    fn webhooks_without_matching_rules_or_rules_are_skipped() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        let a = attrs(&pods, "CREATE");
        assert!(!webhook_matches(&api, &json!({"name": "h"}), &a, &pod()));
        let rules = json!({"rules": [{"operations": ["DELETE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"]}]});
        assert!(!webhook_matches(&api, &rules, &a, &pod()));
        let rules = json!({"rules": [{"operations": ["CREATE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"]}], "objectSelector": null, "namespaceSelector": null});
        assert!(webhook_matches(&api, &rules, &a, &pod()));
    }

    #[test]
    fn object_selector_matches_the_new_or_the_old_object() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        let hook = |labels: Value| json!({"rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(), "objectSelector": {"matchLabels": labels}});
        let a = attrs(&pods, "CREATE");
        assert!(webhook_matches(&api, &hook(json!({"app": "a"})), &a, &pod()));
        assert!(!webhook_matches(&api, &hook(json!({"app": "b"})), &a, &pod()));
        let old = json!({"metadata": {"name": "p", "labels": {"app": "b"}}});
        let update = Attributes { old: Some(&old), ..attrs(&pods, "UPDATE") };
        assert!(webhook_matches(&api, &hook(json!({"app": "b"})), &update, &pod()));
        assert!(!webhook_matches(&api, &hook(json!({"app": "c"})), &update, &pod()));
        let expr = json!({"rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(), "objectSelector": {"matchExpressions": [{"key": "app", "operator": "Exists"}]}});
        assert!(webhook_matches(&api, &expr, &a, &pod()));
    }

    #[test]
    fn namespace_selector_reads_the_namespace_labels_and_its_name_label() {
        let api = api();
        put(&api, "", "namespaces", "", json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "prod", "labels": {"env": "prod"}}}));
        put(&api, "", "namespaces", "", json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "bare"}}));
        let pods = api.resource("", "pods").unwrap();
        let hook = |labels: Value| json!({"rules": [{"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]}], "namespaceSelector": {"matchLabels": labels}});
        let in_ns = |ns: &'static str| Attributes { namespace: ns, ..attrs(&pods, "CREATE") };
        assert!(webhook_matches(&api, &hook(json!({"env": "prod"})), &in_ns("prod"), &pod()));
        assert!(!webhook_matches(&api, &hook(json!({"env": "prod"})), &in_ns("bare"), &pod()));
        assert!(webhook_matches(&api, &hook(json!({"kubernetes.io/metadata.name": "bare"})), &in_ns("bare"), &pod()));
        assert!(webhook_matches(&api, &hook(json!({"kubernetes.io/metadata.name": "ghost"})), &in_ns("ghost"), &pod()));
        assert!(!webhook_matches(&api, &hook(json!({"env": "prod"})), &in_ns("ghost"), &pod()));
        // An empty selector matches everything.
        assert!(webhook_matches(&api, &json!({"rules": [{"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]}], "namespaceSelector": {}}), &in_ns("ghost"), &pod()));

        // Cluster-scoped objects are not filtered, a Namespace by its own labels.
        let nodes = api.resource("", "nodes").unwrap();
        let node = Attributes { namespace: "", ..attrs(&nodes, "CREATE") };
        assert!(webhook_matches(&api, &hook(json!({"env": "prod"})), &node, &json!({"metadata": {"name": "n1"}})));
        let namespaces = api.resource("", "namespaces").unwrap();
        let ns = Attributes { namespace: "", name: "new", ..attrs(&namespaces, "CREATE") };
        assert!(webhook_matches(&api, &hook(json!({"env": "dev"})), &ns, &json!({"metadata": {"name": "new", "labels": {"env": "dev"}}})));
        assert!(!webhook_matches(&api, &hook(json!({"env": "prod"})), &ns, &json!({"metadata": {"name": "new", "labels": {"env": "dev"}}})));
        assert!(webhook_matches(&api, &hook(json!({"kubernetes.io/metadata.name": "new"})), &ns, &json!({"metadata": {"name": "new"}})));
    }

    #[test]
    fn match_conditions_skip_a_webhook_only_when_one_is_false() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        let hook = |exprs: &[&str]| {
            let conds: Vec<Value> = exprs.iter().enumerate().map(|(i, e)| json!({"name": format!("c{i}"), "expression": e})).collect();
            json!({"rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(), "matchConditions": conds})
        };
        let a = attrs(&pods, "CREATE");
        assert!(webhook_matches(&api, &hook(&["object.metadata.name == 'p'", "request.operation == 'CREATE'"]), &a, &pod()));
        assert!(!webhook_matches(&api, &hook(&["object.metadata.name == 'p'", "request.namespace == 'other'"]), &a, &pod()));
        // An error still calls the webhook.
        assert!(webhook_matches(&api, &hook(&["object.metadata.nope.deeper == 1"]), &a, &pod()));
        assert!(webhook_matches(&api, &json!({"rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(), "matchConditions": []}), &a, &pod()));
        let old = pod();
        let delete = Attributes { old: Some(&old), ..attrs(&pods, "DELETE") };
        assert!(webhook_matches(&api, &hook(&["object == null && oldObject.metadata.name == 'p'"]), &delete, &pod()));
        let dry = Attributes { dry_run: true, sub: Some("status"), ..attrs(&pods, "UPDATE") };
        assert!(webhook_matches(&api, &hook(&["request.dryRun && request.subResource == 'status' && request.userInfo.username == 'system:admin'"]), &dry, &pod()));
    }

    #[tokio::test]
    async fn admit_without_configurations_or_for_webhook_configurations_returns_the_object() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        assert_eq!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap(), pod());
        // A configuration whose webhook cannot be reached does not apply to its own group.
        webhook_config(&api, false, "v", json!([{"name": "v.example.com", "clientConfig": {"url": "https://127.0.0.1:1/"}, "rules": [{"operations": ["*"], "apiGroups": ["*"], "apiVersions": ["*"], "resources": ["*"]}]}]));
        let vwc = api.resource("admissionregistration.k8s.io", "validatingwebhookconfigurations").unwrap();
        let obj = json!({"metadata": {"name": "x"}});
        assert_eq!(admit(&api, &Attributes { namespace: "", ..attrs(&vwc, "CREATE") }, obj.clone()).await.unwrap(), obj);
        assert!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.is_err());
    }

    #[tokio::test]
    async fn mutating_webhooks_patch_in_name_order_and_get_an_admission_review() {
        let api = api();
        let s = server(
            Duration::ZERO,
            Box::new(|path: &str, body: &Value| {
                let op = if path.starts_with("/first") {
                    json!([{"op": "add", "path": "/metadata/labels/first", "value": "yes"}])
                } else {
                    // Sees the first webhook's change.
                    let seen = body.pointer("/request/object/metadata/labels/first").cloned().unwrap_or_default();
                    json!([{"op": "add", "path": "/metadata/labels/second", "value": seen}])
                };
                review(body, json!({"allowed": true, "patchType": "JSONPatch", "patch": b64_patch(op)}))
            }),
        )
        .await;
        webhook_config(&api, true, "b-second", json!([url_hook("second.example.com", &s, "/second", json!({"timeoutSeconds": 7}))]));
        webhook_config(&api, true, "a-first", json!([url_hook("first.example.com", &s, "/first?x=1", json!({}))]));
        let pods = api.resource("", "pods").unwrap();
        let out = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap();
        assert_eq!(out["metadata"]["labels"], json!({"app": "a", "first": "yes", "second": "yes"}));

        let seen = s.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "/first?x=1&timeout=10s");
        assert_eq!(seen[1].0, "/second?timeout=7s");
        let review = &seen[0].1;
        assert_eq!(review["apiVersion"], "admission.k8s.io/v1");
        assert_eq!(review["kind"], "AdmissionReview");
        let req = &review["request"];
        assert_eq!(req["uid"].as_str().unwrap().len(), 32);
        assert_ne!(req["uid"], seen[1].1["request"]["uid"]);
        assert_eq!(req["kind"], json!({"group": "", "version": "v1", "kind": "Pod"}));
        assert_eq!(req["resource"], json!({"group": "", "version": "v1", "resource": "pods"}));
        assert_eq!(req["requestKind"], req["kind"]);
        assert_eq!(req["requestResource"], req["resource"]);
        assert_eq!((req["name"].as_str(), req["namespace"].as_str(), req["operation"].as_str()), (Some("p"), Some("default"), Some("CREATE")));
        assert_eq!(req["object"], pod());
        assert_eq!(req["oldObject"], Value::Null);
        assert_eq!(req["dryRun"], false);
        assert_eq!(req["options"]["kind"], "CreateOptions");
        assert_eq!(req["userInfo"]["username"], "system:admin");
        assert!(req.get("subResource").is_none());
    }

    #[tokio::test]
    async fn update_and_delete_requests_carry_the_old_object_and_subresource() {
        let api = api();
        let s = server(Duration::ZERO, Box::new(|_: &str, body: &Value| review(body, json!({"allowed": true})))).await;
        webhook_config(&api, false, "v", json!([url_hook("v.example.com", &s, "", json!({}))]));
        let pods = api.resource("", "pods").unwrap();
        let old = json!({"metadata": {"name": "p", "labels": {"app": "old"}}});
        let update = Attributes { old: Some(&old), sub: Some("status"), dry_run: true, ..attrs(&pods, "UPDATE") };
        admit(&api, &update, pod()).await.unwrap();
        let delete = Attributes { old: Some(&old), ..attrs(&pods, "DELETE") };
        admit(&api, &delete, pod()).await.unwrap();
        let seen = s.seen.lock().unwrap().clone();
        assert_eq!(seen[0].0, "/?timeout=10s");
        let req = &seen[0].1["request"];
        assert_eq!(req["subResource"], "status");
        assert_eq!(req["oldObject"], old);
        assert_eq!(req["dryRun"], true);
        assert_eq!(req["options"]["kind"], "UpdateOptions");
        let req = &seen[1].1["request"];
        assert_eq!(req["object"], Value::Null);
        assert_eq!(req["oldObject"], old);
        assert_eq!(req["operation"], "DELETE");
        assert_eq!(req["options"]["kind"], "DeleteOptions");
    }

    #[tokio::test]
    async fn validating_webhooks_deny_with_the_api_server_wording() {
        let cases = [
            (json!({"code": 403, "message": "no privileged pods"}), StatusCode::FORBIDDEN, "Forbidden", "admission webhook \"v.example.com\" denied the request: no privileged pods"),
            (json!({"code": 422, "reason": "bad spec"}), StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "admission webhook \"v.example.com\" denied the request: bad spec"),
            (json!({"code": 404, "message": ""}), StatusCode::NOT_FOUND, "NotFound", "admission webhook \"v.example.com\" denied the request without explanation"),
            (json!({"code": 409, "message": "taken"}), StatusCode::CONFLICT, "Conflict", "admission webhook \"v.example.com\" denied the request: taken"),
            (json!({"code": 200, "message": "low code"}), StatusCode::BAD_REQUEST, "BadRequest", "admission webhook \"v.example.com\" denied the request: low code"),
            (json!({"code": 503, "message": "other code"}), StatusCode::SERVICE_UNAVAILABLE, "BadRequest", "admission webhook \"v.example.com\" denied the request: other code"),
            (Value::Null, StatusCode::BAD_REQUEST, "BadRequest", "admission webhook \"v.example.com\" denied the request without explanation"),
        ];
        for (status, code, reason, message) in cases {
            let api = api();
            let reply = json!({"allowed": false, "status": status});
            let s = server(Duration::ZERO, Box::new(move |_: &str, body: &Value| review(body, reply.clone()))).await;
            webhook_config(&api, false, "v", json!([url_hook("v.example.com", &s, "/validate", json!({}))]));
            let pods = api.resource("", "pods").unwrap();
            let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
            assert_eq!((err.code, err.reason, err.message.as_str()), (code, reason, message));
        }
    }

    #[tokio::test]
    async fn validating_webhooks_do_not_patch() {
        let api = api();
        let patch = b64_patch(json!([{"op": "add", "path": "/metadata/labels/x", "value": "y"}]));
        let s = server(Duration::ZERO, Box::new(move |_: &str, body: &Value| review(body, json!({"allowed": true, "patch": patch})))).await;
        webhook_config(&api, false, "v", json!([url_hook("v.example.com", &s, "/", json!({}))]));
        let pods = api.resource("", "pods").unwrap();
        assert_eq!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap(), pod());
    }

    #[tokio::test]
    async fn invalid_or_undecodable_patches() {
        let api = api();
        let bad = b64_patch(json!([{"op": "replace", "path": "/spec/nothing/here", "value": 1}]));
        let s = server(Duration::ZERO, Box::new(move |_: &str, body: &Value| review(body, json!({"allowed": true, "patch": bad})))).await;
        webhook_config(&api, true, "m", json!([url_hook("m.example.com", &s, "/", json!({}))]));
        let pods = api.resource("", "pods").unwrap();
        let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
        assert_eq!((err.code, err.reason), (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"));
        assert!(err.message.starts_with("admission webhook \"m.example.com\" returned an invalid patch: "), "{}", err.message);

        // Not base64, or not a JSON array: ignored.
        for patch in [json!("%%%"), json!(b64_patch(json!({"op": "add"})))] {
            let api = self::api();
            let s = server(Duration::ZERO, Box::new(move |_: &str, body: &Value| review(body, json!({"allowed": true, "patch": patch})))).await;
            webhook_config(&api, true, "m", json!([url_hook("m.example.com", &s, "/", json!({}))]));
            assert_eq!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap(), pod());
        }
    }

    #[tokio::test]
    async fn failure_policy_ignore_skips_unreachable_or_broken_webhooks() {
        let s = server(Duration::ZERO, Box::new(|_: &str, _: &Value| (500, json!({"error": "boom"})))).await;
        let pods = api().resource("", "pods").unwrap();
        let hooks = [
            json!({"name": "down.example.com", "clientConfig": {"url": "https://127.0.0.1:1/"}, "rules": serde_json::from_str::<Value>(ALL_PODS).unwrap()}),
            url_hook("broken.example.com", &s, "/", json!({})),
            json!({"name": "plain.example.com", "clientConfig": {"url": "http://127.0.0.1:1/"}, "rules": serde_json::from_str::<Value>(ALL_PODS).unwrap()}),
            json!({"name": "nosvc.example.com", "clientConfig": {"service": {"namespace": "default", "name": "missing"}}, "rules": serde_json::from_str::<Value>(ALL_PODS).unwrap()}),
        ];
        let wanted = [
            "failed to call webhook: Post \"https://127.0.0.1:1/?timeout=10s\": failed to connect to 127.0.0.1:1",
            "webhook answered 500 Internal Server Error: {\"error\":\"boom\"}",
            "webhook URL must be https",
            "service \"default/missing\" not found",
        ];
        for (hook, want) in hooks.iter().zip(wanted) {
            for (policy, mutating) in [("Ignore", true), ("Ignore", false), ("Fail", true), ("Fail", false)] {
                let api = api();
                let mut hook = hook.clone();
                hook["failurePolicy"] = json!(policy);
                let name = util::s(&hook, "/name").to_string();
                webhook_config(&api, mutating, "c", json!([hook]));
                let result = admit(&api, &attrs(&pods, "CREATE"), pod()).await;
                if policy == "Ignore" {
                    assert_eq!(result.unwrap(), pod());
                    continue;
                }
                let err = result.unwrap_err();
                assert_eq!((err.code, err.reason), (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"));
                let prefix = format!("Internal error occurred: failed calling webhook \"{name}\": ");
                assert!(err.message.starts_with(&prefix) && err.message.contains(want), "{}", err.message);
            }
        }
    }

    #[tokio::test]
    async fn a_webhook_without_a_response_or_with_bad_json_fails() {
        let pods = api().resource("", "pods").unwrap();
        let s = server(Duration::ZERO, Box::new(|_: &str, _: &Value| (200, json!({"kind": "AdmissionReview"})))).await;
        let api = api();
        webhook_config(&api, false, "v", json!([url_hook("v.example.com", &s, "/", json!({}))]));
        let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
        assert!(err.message.ends_with("webhook returned no response"), "{}", err.message);
    }

    #[tokio::test]
    async fn slow_webhooks_time_out_after_timeout_seconds() {
        let api = api();
        let s = server(Duration::from_millis(1500), Box::new(|_: &str, body: &Value| review(body, json!({"allowed": true})))).await;
        // 0 is raised to the 1 s minimum.
        webhook_config(&api, false, "v", json!([url_hook("v.example.com", &s, "/slow", json!({"timeoutSeconds": 0}))]));
        let pods = api.resource("", "pods").unwrap();
        let started = std::time::Instant::now();
        let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(1400));
        let url = format!("https://localhost:{}/slow?timeout=1s", s.port);
        assert_eq!(err.message, format!("Internal error occurred: failed calling webhook \"v.example.com\": failed to call webhook: Post \"{url}\": context deadline exceeded"));
    }

    #[tokio::test]
    async fn the_ca_bundle_is_checked() {
        let api = api();
        let s = server(Duration::ZERO, Box::new(|_: &str, body: &Value| review(body, json!({"allowed": true})))).await;
        let other = server(Duration::ZERO, Box::new(|_: &str, body: &Value| review(body, json!({"allowed": true})))).await;
        let mut hook = url_hook("v.example.com", &s, "/", json!({}));
        hook["clientConfig"]["caBundle"] = json!(other.ca);
        webhook_config(&api, false, "v", json!([hook]));
        let pods = api.resource("", "pods").unwrap();
        assert!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.is_err());
        // Without one, the system roots do not know the test CA either.
        let api = self::api();
        let mut hook = url_hook("v.example.com", &s, "/", json!({}));
        hook["clientConfig"].as_object_mut().unwrap().remove("caBundle");
        webhook_config(&api, false, "v", json!([hook]));
        assert!(admit(&api, &attrs(&pods, "CREATE"), pod()).await.is_err());
        assert!(s.seen.lock().unwrap().is_empty());
    }

    fn service(api: &ApiState, port: u16, endpoints: Value) {
        put(api, "", "services", "default", json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "hook", "namespace": "default"}, "spec": {"ports": [{"name": "metrics", "port": 9090}, {"name": "https", "port": 443, "targetPort": port}]}}));
        if !endpoints.is_null() {
            put(api, "", "endpoints", "default", json!({"apiVersion": "v1", "kind": "Endpoints", "metadata": {"name": "hook", "namespace": "default"}, "subsets": endpoints}));
        }
    }

    #[tokio::test]
    async fn service_webhooks_are_called_at_a_ready_endpoint() {
        let api = api();
        let s = server(Duration::ZERO, Box::new(|_: &str, body: &Value| review(body, json!({"allowed": false, "status": {"message": "from the service"}})))).await;
        service(
            &api,
            s.port,
            json!([
                {"notReadyAddresses": [{"ip": "10.0.0.9"}], "ports": [{"name": "https", "port": 1}]},
                {"addresses": [{"ip": "127.0.0.1"}], "ports": [{"name": "metrics", "port": 1}, {"name": "https", "port": s.port}]},
            ]),
        );
        let hook = json!({
            "name": "svc.example.com",
            "clientConfig": {"service": {"namespace": "default", "name": "hook", "path": "/validate"}, "caBundle": s.ca},
            "rules": serde_json::from_str::<Value>(ALL_PODS).unwrap(),
        });
        webhook_config(&api, false, "v", json!([hook]));
        let pods = api.resource("", "pods").unwrap();
        let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
        assert_eq!(err.message, "admission webhook \"svc.example.com\" denied the request: from the service");
        assert_eq!(s.seen.lock().unwrap()[0].0, "/validate?timeout=10s");
    }

    #[test]
    fn resolve_service_picks_the_named_port_of_a_ready_address() {
        let api = api();
        let err = |r: Result<Backend>| format!("{:#}", r.err().unwrap());
        assert_eq!(err(resolve_service(&api, "default", "hook", 443)), "service \"default/hook\" not found");
        service(&api, 8443, Value::Null);
        assert_eq!(err(resolve_service(&api, "default", "hook", 443)), "no endpoints for service \"default/hook\"");

        let api = self::api();
        service(&api, 8443, json!([{"notReadyAddresses": [{"ip": "10.0.0.9"}], "ports": [{"port": 1}]}, {"addresses": [{"ip": "10.0.0.8"}]}]));
        assert_eq!(err(resolve_service(&api, "default", "hook", 443)), "no ready endpoints for service \"default/hook\"");

        let api = self::api();
        service(
            &api,
            8443,
            json!([{"addresses": [{"ip": "10.0.0.7", "nodeName": "n1", "targetRef": {"kind": "Pod", "uid": "u-1"}}], "ports": [{"name": "metrics", "port": 9100}, {"name": "https", "port": 8443}]}]),
        );
        let b = resolve_service(&api, "default", "hook", 443).unwrap();
        assert_eq!((b.addr.as_str(), b.node.as_str(), b.pod_uid.as_str(), b.port), ("10.0.0.7:8443", "n1", "u-1", 8443));
        // An unknown port falls back to the first one.
        let b = resolve_service(&api, "default", "hook", 1234).unwrap();
        assert_eq!(b.addr, "10.0.0.7:9100");

        let api = self::api();
        put(&api, "", "services", "default", json!({"metadata": {"name": "hook", "namespace": "default"}, "spec": {}}));
        assert_eq!(err(resolve_service(&api, "default", "hook", 443)), "service \"default/hook\" has no port 443");
    }

    #[tokio::test]
    async fn the_tunnel_is_not_used_for_backends_without_a_pod() {
        let api = api();
        let backend = Backend { addr: "127.0.0.1:1".into(), ..Default::default() };
        let e = dial(&api, &backend).await.err().unwrap();
        assert_eq!(format!("{e}"), "failed to connect to 127.0.0.1:1");
    }

    fn crd(conversion: Value) -> Value {
        json!({"metadata": {"name": "widgets.example.com"}, "spec": {"group": "example.com", "conversion": conversion}})
    }

    fn widget(version: &str, name: &str) -> Value {
        json!({"apiVersion": version, "kind": "Widget", "metadata": {"name": name, "uid": format!("uid-{name}"), "labels": {"l": name}, "annotations": {"a": name}}, "spec": {"size": 1}})
    }

    #[tokio::test]
    async fn conversion_without_a_webhook_relabels() {
        let api = api();
        let objects = vec![widget("example.com/v1", "a"), widget("example.com/v2", "b")];
        let out = convert_objects(&api, &crd(json!({"strategy": "None"})), objects.clone(), "example.com/v2").await.unwrap();
        assert_eq!(out[0]["apiVersion"], "example.com/v2");
        assert_eq!(out[0]["spec"], objects[0]["spec"]);
        assert_eq!(out[1], objects[1]);
        // Nothing to convert: the webhook is not called.
        let hook = crd(json!({"strategy": "Webhook", "webhook": {"clientConfig": {"url": "https://127.0.0.1:1/"}}}));
        let same = vec![widget("example.com/v2", "b")];
        assert_eq!(convert_objects(&api, &hook, same.clone(), "example.com/v2").await.unwrap(), same);
    }

    #[tokio::test]
    async fn conversion_webhooks_convert_and_keep_the_metadata() {
        let api = api();
        let s = server(
            Duration::ZERO,
            Box::new(|_: &str, body: &Value| {
                let desired = body.pointer("/request/desiredAPIVersion").cloned().unwrap_or_default();
                let converted: Vec<Value> = body.pointer("/request/objects").and_then(Value::as_array).into_iter().flatten().map(|o| {
                    json!({"apiVersion": desired, "kind": "Widget", "metadata": {"name": "renamed", "labels": {"l": "changed"}}, "spec": {"sizeV2": o["spec"]["size"]}})
                }).collect();
                (200, json!({"apiVersion": body["apiVersion"], "kind": "ConversionReview", "response": {"uid": body["request"]["uid"], "convertedObjects": converted, "result": {"status": "Success"}}}))
            }),
        )
        .await;
        let conversion = json!({"strategy": "Webhook", "webhook": {"conversionReviewVersions": ["v1beta1", "v1"], "clientConfig": {"url": format!("https://localhost:{}/convert", s.port), "caBundle": s.ca}}});
        let objects = vec![widget("example.com/v1", "a"), widget("example.com/v2", "b"), widget("example.com/v1", "c")];
        let out = convert_objects(&api, &crd(conversion), objects.clone(), "example.com/v2").await.unwrap();
        assert_eq!(out[0]["apiVersion"], "example.com/v2");
        assert_eq!(out[0]["spec"], json!({"sizeV2": 1}));
        assert_eq!(out[0]["metadata"], objects[0]["metadata"]);
        assert_eq!(out[1], objects[1]);
        assert_eq!(out[2]["metadata"], objects[2]["metadata"]);
        let seen = s.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "/convert");
        assert_eq!(seen[0].1["apiVersion"], "apiextensions.k8s.io/v1");
        assert_eq!(seen[0].1["request"]["objects"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn webhook_urls_split_into_address_server_name_and_path() {
        let parts = |u: &str| url_parts(u).unwrap();
        assert_eq!(parts("https://hook.example.com/convert?x=1"), ("hook.example.com:443".into(), "hook.example.com".into(), "/convert?x=1".into()));
        assert_eq!(parts("https://10.0.0.1:8443"), ("10.0.0.1:8443".into(), "10.0.0.1".into(), "/".into()));
        assert_eq!(parts("https://[::1]:8443/v"), ("[::1]:8443".into(), "::1".into(), "/v".into()));
        assert!(url_parts("http://hook/").is_err());
    }

    #[tokio::test]
    async fn conversion_webhooks_at_an_ipv6_address() {
        let api = api();
        let handler: Handler = Box::new(|_: &str, body: &Value| {
            let objects = body.pointer("/request/objects").cloned().unwrap_or_default();
            let converted: Vec<Value> = objects.as_array().into_iter().flatten().map(|o| {
                let mut o = o.clone();
                o["apiVersion"] = body["request"]["desiredAPIVersion"].clone();
                o
            }).collect();
            (200, json!({"apiVersion": body["apiVersion"], "kind": "ConversionReview", "response": {"uid": body["request"]["uid"], "convertedObjects": converted, "result": {"status": "Success"}}}))
        });
        let Some(s) = server_on("[::1]:0", Duration::ZERO, handler).await else {
            eprintln!("no IPv6 loopback, skipped");
            return;
        };
        let conversion = json!({"strategy": "Webhook", "webhook": {"clientConfig": {"url": format!("https://[::1]:{}/", s.port), "caBundle": s.ca}}});
        let out = convert_objects(&api, &crd(conversion), vec![widget("example.com/v1", "a")], "example.com/v2").await.unwrap();
        assert_eq!(out[0]["apiVersion"], "example.com/v2");
    }

    #[tokio::test]
    async fn conversion_webhook_failures() {
        let api = api();
        let s = server(
            Duration::ZERO,
            Box::new(|path: &str, body: &Value| match path {
                "/fail" => (200, json!({"response": {"result": {"status": "Failure", "message": "cannot convert"}}})),
                "/fail-quietly" => (200, json!({"response": {"result": {"status": "Failure"}}})),
                "/short" => (200, json!({"response": {"convertedObjects": []}})),
                _ => (200, json!({"apiVersion": body["apiVersion"]})),
            }),
        )
        .await;
        let objects = vec![widget("example.com/v1", "a")];
        let run = |config: Value| {
            let api = api.clone();
            let objects = objects.clone();
            async move {
                let conversion = json!({"strategy": "Webhook", "webhook": {"clientConfig": config}});
                convert_objects(&api, &crd(conversion), objects, "example.com/v2").await.unwrap_err().message
            }
        };
        let url = |p: &str| json!({"url": format!("https://localhost:{}{p}", s.port), "caBundle": s.ca});
        assert_eq!(run(url("/fail")).await, "conversion webhook for widgets.example.com failed: cannot convert");
        assert_eq!(run(url("/fail-quietly")).await, "conversion webhook for widgets.example.com failed: conversion failed");
        assert_eq!(run(url("/short")).await, "conversion webhook for widgets.example.com failed: returned 0 objects for 1");
        assert_eq!(run(json!({"url": "http://localhost/"})).await, "conversion webhook for widgets.example.com failed: webhook URL must be https");
        assert_eq!(run(json!({"service": {"namespace": "default", "name": "hook"}})).await, "conversion webhook for widgets.example.com failed: service \"default/hook\" not found");
        // Without `v1` among conversionReviewVersions, a v1beta1 review; no convertedObjects at all.
        let conversion = json!({"strategy": "Webhook", "webhook": {"clientConfig": url("/")}});
        let err = convert_objects(&api, &crd(conversion), objects.clone(), "example.com/v2").await.unwrap_err();
        assert_eq!(err.message, "conversion webhook for widgets.example.com failed: returned 0 objects for 1");
        assert_eq!(s.seen.lock().unwrap().last().unwrap().1["apiVersion"], "apiextensions.k8s.io/v1beta1");
    }

    #[tokio::test]
    async fn conversion_webhooks_behind_a_service() {
        let api = api();
        let s = server(
            Duration::ZERO,
            Box::new(|_: &str, body: &Value| {
                let objs: Vec<Value> = body["request"]["objects"].as_array().unwrap().iter().map(|o| {
                    let mut o = o.clone();
                    o["apiVersion"] = body["request"]["desiredAPIVersion"].clone();
                    o
                }).collect();
                (200, json!({"response": {"convertedObjects": objs}}))
            }),
        )
        .await;
        service(&api, s.port, json!([{"addresses": [{"ip": "127.0.0.1"}], "ports": [{"name": "https", "port": s.port}]}]));
        let conversion = json!({"strategy": "Webhook", "webhook": {"clientConfig": {"service": {"namespace": "default", "name": "hook", "port": 443, "path": "/convert"}, "caBundle": s.ca}}});
        let out = convert_objects(&api, &crd(conversion), vec![widget("example.com/v1", "a")], "example.com/v2").await.unwrap();
        assert_eq!(out[0]["apiVersion"], "example.com/v2");
        assert_eq!(s.seen.lock().unwrap()[0].0, "/convert");
    }

    const GROUP: &str = "admissionregistration.k8s.io";

    fn policy(api: &ApiState, name: &str, spec: Value) {
        put(api, GROUP, "validatingadmissionpolicies", "", json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicy", "metadata": {"name": name}, "spec": spec}));
    }

    fn binding(api: &ApiState, name: &str, spec: Value) {
        put(api, GROUP, "validatingadmissionpolicybindings", "", json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicyBinding", "metadata": {"name": name}, "spec": spec}));
    }

    fn pod_constraints() -> Value {
        json!({"resourceRules": [{"operations": ["CREATE", "UPDATE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"]}]})
    }

    fn denial(api: &ApiState, a: &Attributes, obj: &Value) -> Option<(StatusCode, &'static str, String)> {
        validating_policies(api, a, obj).err().map(|e| (e.code, e.reason, e.message))
    }

    #[test]
    fn policies_deny_with_message_reason_and_message_expression() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        let a = attrs(&pods, "CREATE");
        assert_eq!(denial(&api, &a, &pod()), None);
        policy(
            &api,
            "labels",
            json!({"matchConstraints": pod_constraints(), "validations": [
                {"expression": "has(object.metadata.labels)"},
                {"expression": "object.metadata.labels.app != 'a'", "messageExpression": "'app ' + object.metadata.labels.app + ' is reserved'", "reason": "Forbidden"},
            ]}),
        );
        // A policy without a binding does nothing; a binding to a missing policy neither.
        assert_eq!(denial(&api, &a, &pod()), None);
        binding(&api, "ghost", json!({"policyName": "missing", "validationActions": ["Deny"]}));
        assert_eq!(denial(&api, &a, &pod()), None);
        binding(&api, "labels-binding", json!({"policyName": "labels", "validationActions": ["Deny"]}));
        assert_eq!(
            denial(&api, &a, &pod()),
            Some((StatusCode::FORBIDDEN, "Forbidden", "ValidatingAdmissionPolicy 'labels' with binding 'labels-binding' denied request: app a is reserved".into()))
        );
        let unlabeled = json!({"metadata": {"name": "p", "namespace": "default"}});
        assert_eq!(
            denial(&api, &a, &unlabeled),
            Some((StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "ValidatingAdmissionPolicy 'labels' with binding 'labels-binding' denied request: failed expression: has(object.metadata.labels)".into()))
        );
        let other = json!({"metadata": {"name": "p", "namespace": "default", "labels": {"app": "b"}}});
        assert_eq!(denial(&api, &a, &other), None);
        // Not matched: another resource, an excluded operation.
        let secrets = api.resource("", "secrets").unwrap();
        assert_eq!(denial(&api, &attrs(&secrets, "CREATE"), &unlabeled), None);
        let old = pod();
        assert_eq!(denial(&api, &Attributes { old: Some(&old), ..attrs(&pods, "DELETE") }, &pod()), None);
    }

    #[test]
    fn policy_messages_and_reasons() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        policy(
            &api,
            "p",
            json!({"validations": [
                {"expression": "object.metadata.name != 'unauth'", "message": "who are you", "reason": "Unauthorized"},
                {"expression": "object.metadata.name != 'big'", "message": "too big", "reason": "RequestEntityTooLarge"},
                {"expression": "object.metadata.name != 'blank'", "messageExpression": "' '", "message": "fallback"},
                {"expression": "object.metadata.name != 'broken'", "messageExpression": "1 +", "message": "fallback too"},
            ]}),
        );
        binding(&api, "b", json!({"policyName": "p"}));
        let deny = |name: &'static str| {
            let a = Attributes { name, ..attrs(&pods, "CREATE") };
            denial(&api, &a, &json!({"metadata": {"name": name}})).map(|(code, reason, m)| (code, reason, m.rsplit(": ").next().unwrap().to_string()))
        };
        assert_eq!(deny("unauth"), Some((StatusCode::UNAUTHORIZED, "Unauthorized", "who are you".into())));
        assert_eq!(deny("big"), Some((StatusCode::PAYLOAD_TOO_LARGE, "RequestEntityTooLarge", "too big".into())));
        assert_eq!(deny("blank"), Some((StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "fallback".into())));
        assert_eq!(deny("broken"), Some((StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "fallback too".into())));
        assert_eq!(deny("fine"), None);
    }

    #[test]
    fn policies_with_only_warn_or_audit_actions_do_not_deny() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        policy(&api, "p", json!({"validations": [{"expression": "false"}, {"expression": "1 + 'a'"}]}));
        binding(&api, "b", json!({"policyName": "p", "validationActions": ["Warn", "Audit"]}));
        assert_eq!(denial(&api, &attrs(&pods, "CREATE"), &pod()), None);
    }

    #[test]
    fn policy_errors_follow_the_failure_policy() {
        let pods = api().resource("", "pods").unwrap();
        for (validation, want) in [
            ("object.metadata.missing.field == 1", "expression 'object.metadata.missing.field == 1' resulted in error: "),
            ("object.metadata.name", "expression 'object.metadata.name' resulted in a "),
        ] {
            for fail in ["Fail", "Ignore"] {
                let api = api();
                policy(&api, "p", json!({"failurePolicy": fail, "validations": [{"expression": validation}]}));
                binding(&api, "b", json!({"policyName": "p", "validationActions": ["Deny"]}));
                let got = denial(&api, &attrs(&pods, "CREATE"), &pod());
                if fail == "Ignore" {
                    assert_eq!(got, None);
                    continue;
                }
                let (code, reason, message) = got.unwrap();
                assert_eq!((code, reason), (StatusCode::UNPROCESSABLE_ENTITY, "Invalid"));
                let prefix = format!("ValidatingAdmissionPolicy 'p' with binding 'b' denied request: {want}");
                assert!(message.starts_with(&prefix), "{message}");
            }
        }
        // A failed validation is reported before an error of a later one.
        let api = api();
        policy(&api, "p", json!({"validations": [{"expression": "1 + 'a'"}, {"expression": "false", "message": "plain no"}]}));
        binding(&api, "b", json!({"policyName": "p"}));
        assert_eq!(denial(&api, &attrs(&pods, "CREATE"), &pod()).unwrap().2, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: plain no");
    }

    #[test]
    fn policy_variables_and_match_conditions() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        policy(
            &api,
            "p",
            json!({
                "variables": [
                    {"name": "app", "expression": "object.metadata.labels.app"},
                    {"name": "loud", "expression": "variables.app + '!'"},
                ],
                "matchConditions": [{"name": "only-labeled", "expression": "has(object.metadata.labels)"}],
                "validations": [{"expression": "variables.loud != 'a!'", "messageExpression": "'got ' + variables.loud"}],
            }),
        );
        binding(&api, "b", json!({"policyName": "p"}));
        let a = attrs(&pods, "CREATE");
        assert_eq!(denial(&api, &a, &pod()).unwrap().2, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: got a!");
        // The match condition is false: the policy does not apply.
        assert_eq!(denial(&api, &a, &json!({"metadata": {"name": "p"}})), None);

        let api = self::api();
        policy(&api, "p", json!({"variables": [{"name": "bad", "expression": "object.nope.deeper"}], "validations": [{"expression": "true"}]}));
        binding(&api, "b", json!({"policyName": "p"}));
        let message = denial(&api, &a, &pod()).unwrap().2;
        assert!(message.contains("variable 'bad' resulted in error: "), "{message}");

        let api = self::api();
        policy(&api, "p", json!({"matchConditions": [{"name": "boom", "expression": "object.nope.deeper"}], "validations": [{"expression": "true"}]}));
        binding(&api, "b", json!({"policyName": "p"}));
        let message = denial(&api, &a, &pod()).unwrap().2;
        assert!(message.contains("matchCondition 'boom' resulted in error: "), "{message}");
    }

    #[test]
    fn policy_and_binding_match_resources_and_exclusions() {
        let api = api();
        put(&api, "", "namespaces", "", json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "prod", "labels": {"env": "prod"}}}));
        let pods = api.resource("", "pods").unwrap();
        let mut constraints = pod_constraints();
        constraints["excludeResourceRules"] = json!([{"operations": ["UPDATE"], "apiGroups": [""], "apiVersions": ["v1"], "resources": ["pods"]}]);
        policy(&api, "p", json!({"matchConstraints": constraints, "validations": [{"expression": "namespaceObject.metadata.name != 'prod'", "message": "not in prod"}]}));
        binding(&api, "b", json!({"policyName": "p", "matchResources": {"namespaceSelector": {"matchLabels": {"env": "prod"}}, "objectSelector": {"matchLabels": {"app": "a"}}}}));
        let in_prod = |op: &'static str| Attributes { namespace: "prod", ..attrs(&pods, op) };
        assert_eq!(denial(&api, &in_prod("CREATE"), &pod()).unwrap().2, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: not in prod");
        let old = pod();
        assert_eq!(denial(&api, &Attributes { old: Some(&old), ..in_prod("UPDATE") }, &pod()), None);
        assert_eq!(denial(&api, &attrs(&pods, "CREATE"), &pod()), None);
        assert_eq!(denial(&api, &in_prod("CREATE"), &json!({"metadata": {"name": "p", "labels": {"app": "b"}}})), None);
    }

    #[test]
    fn policy_params_come_from_the_param_ref() {
        let api = api();
        let pods = api.resource("", "pods").unwrap();
        put(&api, "", "configmaps", "default", json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "limits", "namespace": "default"}, "data": {"blocked": "p"}}));
        put(&api, "", "configmaps", "other", json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "limits", "namespace": "other"}, "data": {"blocked": "q"}}));
        policy(
            &api,
            "p",
            json!({"paramKind": {"apiVersion": "v1", "kind": "ConfigMap"}, "validations": [{"expression": "object.metadata.name != params.data.blocked", "messageExpression": "'blocked by ' + params.metadata.namespace"}]}),
        );
        let a = attrs(&pods, "CREATE");
        // The request's namespace by default.
        binding(&api, "b", json!({"policyName": "p", "paramRef": {"name": "limits"}}));
        assert_eq!(denial(&api, &a, &pod()).unwrap().2, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: blocked by default");

        let with_binding = |spec: Value| {
            let api = self::api();
            put(&api, "", "configmaps", "other", json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "limits", "namespace": "other"}, "data": {"blocked": "p"}}));
            policy(&api, "p", json!({"paramKind": {"apiVersion": "v1", "kind": "ConfigMap"}, "validations": [{"expression": "object.metadata.name != params.data.blocked"}]}));
            binding(&api, "b", spec);
            denial(&api, &attrs(&pods, "CREATE"), &pod()).map(|d| d.2)
        };
        assert!(with_binding(json!({"policyName": "p", "paramRef": {"name": "limits", "namespace": "other"}})).is_some());
        assert_eq!(with_binding(json!({"policyName": "p", "paramRef": {"name": "limits", "parameterNotFoundAction": "Allow"}})), None);
        assert_eq!(
            with_binding(json!({"policyName": "p", "paramRef": {"name": "limits", "parameterNotFoundAction": "Deny"}})).unwrap(),
            "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: failed to configure binding: no params found for policy binding with `Deny` parameterNotFoundAction"
        );
        assert_eq!(with_binding(json!({"policyName": "p", "paramRef": {"name": "limits"}, "validationActions": ["Warn"]})), None);
        assert!(with_binding(json!({"policyName": "p"})).is_some());
    }

    #[test]
    fn cluster_scoped_params_and_policy_on_delete() {
        let api = api();
        let nodes = api.resource("", "nodes").unwrap();
        policy(
            &api,
            "p",
            json!({"paramKind": {"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicy"}, "validations": [{"expression": "object == null && oldObject.metadata.name != params.metadata.name", "message": "keep it"}]}),
        );
        binding(&api, "b", json!({"policyName": "p", "paramRef": {"name": "p"}}));
        let old = json!({"metadata": {"name": "p"}});
        let a = Attributes { namespace: "", old: Some(&old), ..attrs(&nodes, "DELETE") };
        assert_eq!(denial(&api, &a, &old).unwrap().2, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: keep it");
        let other = json!({"metadata": {"name": "n2"}});
        assert_eq!(denial(&api, &Attributes { old: Some(&other), ..a }, &other), None);
    }

    #[tokio::test]
    async fn admit_runs_validating_policies_after_webhooks() {
        let api = api();
        let patch = b64_patch(json!([{"op": "add", "path": "/metadata/labels/team", "value": "x"}]));
        let s = server(Duration::ZERO, Box::new(move |_: &str, body: &Value| review(body, json!({"allowed": true, "patch": patch})))).await;
        webhook_config(&api, true, "m", json!([url_hook("m.example.com", &s, "/", json!({}))]));
        policy(&api, "p", json!({"validations": [{"expression": "object.metadata.labels.team != 'x'", "message": "team x is full"}]}));
        binding(&api, "b", json!({"policyName": "p"}));
        let pods = api.resource("", "pods").unwrap();
        let err = admit(&api, &attrs(&pods, "CREATE"), pod()).await.unwrap_err();
        assert_eq!(err.message, "ValidatingAdmissionPolicy 'p' with binding 'b' denied request: team x is full");
    }
}
