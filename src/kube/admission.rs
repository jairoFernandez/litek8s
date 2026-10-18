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
    let (backend, server_name, path) = match config.get("url").and_then(Value::as_str) {
        Some(url) => {
            let rest = url.strip_prefix("https://").context("webhook URL must be https")?;
            let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
            let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority).trim_matches(['[', ']']).to_string();
            let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:443") };
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
            let rest = url.strip_prefix("https://").ok_or_else(|| failed(anyhow::anyhow!("webhook URL must be https")))?;
            let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
            let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority).to_string();
            let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:443") };
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
    let tunnel = !backend.pod_uid.is_empty() && api.hub.is_connected(&backend.node);
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
    let (pod_uid, port) = (backend.pod_uid.clone(), backend.port);
    let mut handle = api.hub.open_stream(&backend.node, |stream| Message::PortForward { stream, pod_uid, port }).await?;
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
}
