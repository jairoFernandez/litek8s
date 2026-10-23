//! Per-kind validation and the built-in admission plugins the conformance
//! suite relies on (LimitRanger, RuntimeClass). Errors use the field-path
//! format of the real API server (`spec.x: Invalid value: "y": reason`) so
//! clients matching on the message behave the same.

use serde_json::{Value, json};

use super::cel::lib::{is_dns1035_label, is_dns1123_label, is_dns1123_subdomain, is_label_value, is_qualified_name, mask_trailing_dash};
use super::quantity;
use super::selector::LabelSelector;

/// Field errors for a create (`old` is `None`) or an update of `obj`.
pub fn validate(group: &str, kind: &str, obj: &Value, old: Option<&Value>) -> Vec<String> {
    let mut errs = Vec::new();
    object_meta(group, kind, obj, &mut errs);
    match (group, kind) {
        ("apps", "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet") => workload(kind, obj, &mut errs),
        ("batch", "Job") => {
            let template = obj.pointer("/spec/template");
            pod_template(template, "spec.template", &mut errs);
            job_restart_policy(template, "spec.template.spec.restartPolicy", &mut errs);
        }
        ("batch", "CronJob") => {
            let template = obj.pointer("/spec/jobTemplate/spec/template");
            pod_template(template, "spec.jobTemplate.spec.template", &mut errs);
            job_restart_policy(template, "spec.jobTemplate.spec.template.spec.restartPolicy", &mut errs);
        }
        ("", "PodTemplate") => pod_template(obj.get("template"), "template", &mut errs),
        ("", "ReplicationController") => pod_template(obj.pointer("/spec/template"), "spec.template", &mut errs),
        ("", "Pod") => pod_spec(obj.get("spec"), "spec", &mut errs),
        ("", "Service") => service(obj, &mut errs),
        _ => {}
    }
    match (group, kind) {
        ("", "ConfigMap" | "Secret") => {
            for field in ["data", "binaryData", "stringData"] {
                for key in obj.get(field).and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k) {
                    let ok = !key.is_empty()
                        && key.len() <= 253
                        && key != "."
                        && key != ".."
                        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                    if !ok {
                        errs.push(format!(
                            "{field}[{key}]: Invalid value: \"{key}\": a valid config key must consist of alphanumeric characters, '-', '_' or '.' (e.g. 'key.name',  or 'KEY_NAME',  or 'key-name', regex used for validation is '[-._a-zA-Z0-9]+')"
                        ));
                    }
                }
            }
        
            if let Some(old) = old.filter(|o| o.get("immutable") == Some(&json!(true))) {
                if obj.get("immutable") != Some(&json!(true)) {
                    errs.push("immutable: Forbidden: field is immutable when `immutable` is set".to_string());
                }
                for field in ["data", "binaryData"] {
                    let empty = json!({});
                    let a = old.get(field).filter(|v| !v.is_null()).unwrap_or(&empty);
                    let b = obj.get(field).filter(|v| !v.is_null()).unwrap_or(&empty);
                    if a != b {
                        errs.push(format!("{field}: Forbidden: field is immutable when `immutable` is set"));
                    }
                }
            }
        }
        ("admissionregistration.k8s.io", "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration") => {
            for (i, hook) in obj.get("webhooks").and_then(Value::as_array).into_iter().flatten().enumerate() {
                for (j, cond) in hook.get("matchConditions").and_then(Value::as_array).into_iter().flatten().enumerate() {
                    let expr = cond.get("expression").and_then(Value::as_str).unwrap_or_default();
                    if let Err(e) = super::cel::compile(expr) {
                        errs.push(format!("webhooks[{i}].matchConditions[{j}].expression: Invalid value: \"{expr}\": compilation failed: {e}"));
                    }
                }
            }
        }
        ("scheduling.k8s.io", "PriorityClass") => {
            if let Some(old) = old {
                if old.get("value") != obj.get("value") {
                    errs.push("value: Forbidden: may not be changed in an update.".to_string());
                }
                if old.get("preemptionPolicy").is_some() && old.get("preemptionPolicy") != obj.get("preemptionPolicy") {
                    errs.push("preemptionPolicy: Invalid value: field is immutable".to_string());
                }
            }
        }
        ("", "Pod") if old.is_none() => {
            for field in ["initContainers", "containers"] {
                for (i, c) in obj.pointer(&format!("/spec/{field}")).and_then(Value::as_array).into_iter().flatten().enumerate() {
                    let limits = c.pointer("/resources/limits").and_then(Value::as_object);
                    for (res, req) in c.pointer("/resources/requests").and_then(Value::as_object).into_iter().flatten() {
                        let limit = limits.and_then(|l| l.get(res));
                        let (Some(r), Some(l)) = (quantity::milli_of(req), limit.and_then(quantity::milli_of)) else { continue };
                        if r > l {
                            errs.push(format!(
                                "spec.{field}[{i}].resources.requests: Invalid value: \"{}\": must be less than or equal to {res} limit of {}",
                                req.as_str().unwrap_or_default(),
                                limit.and_then(Value::as_str).unwrap_or_default()
                            ));
                        }
                    }
                }
            }
            let sysctls = obj.pointer("/spec/securityContext/sysctls").and_then(Value::as_array);
            for (i, s) in sysctls.into_iter().flatten().enumerate() {
                let name = s.get("name").and_then(Value::as_str).unwrap_or_default();
                if !valid_sysctl_name(name) {
                    errs.push(format!(
                        "spec.securityContext.sysctls[{i}].name: Invalid value: \"{name}\": must have at most 253 characters and match regex {SYSCTL_FMT}"
                    ));
                }
            }
        }
        _ => {}
    }
    errs
}

/// A value as Go's `%q` prints it in a field error.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A `map[string]string` as Go's `%#v` prints it (sorted keys).
fn go_map(map: Option<&Value>) -> String {
    let Some(map) = map.and_then(Value::as_object) else { return "map[string]string(nil)".into() };
    let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
    pairs.sort_by_key(|(k, _)| *k);
    let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{}:{}", quote(k), quote(v.as_str().unwrap_or_default()))).collect();
    format!("map[string]string{{{}}}", body.join(", "))
}

/// The name rule `ValidateObjectMeta` applies per kind. Kinds whose names
/// only have to be path segments (RBAC, HPA, flow control) or have a
/// format of their own (CRDs, APIServices, IPAddresses, ...) are left to
/// the path-segment check in `prepare_create`.
fn name_rule(group: &str, kind: &str) -> Option<fn(&str) -> Vec<String>> {
    Some(match (group, kind) {
        ("", "Namespace") => is_dns1123_label,
        ("", "Service") => is_dns1035_label,
        ("rbac.authorization.k8s.io" | "flowcontrol.apiserver.k8s.io" | "apiextensions.k8s.io" | "apiregistration.k8s.io", _)
        | ("certificates.k8s.io" | "authentication.k8s.io" | "authorization.k8s.io", _)
        | ("autoscaling", "HorizontalPodAutoscaler")
        | ("networking.k8s.io", "IPAddress" | "ServiceCIDR") => return None,
        _ => is_dns1123_subdomain,
    })
}

/// `ValidateObjectMeta`: the name (and generateName) by the kind's rule,
/// label and annotation keys and values.
fn object_meta(group: &str, kind: &str, obj: &Value, errs: &mut Vec<String>) {
    let meta = obj.get("metadata");
    let field = |f: &str| meta.and_then(|m| m.get(f));
    if let Some(rule) = name_rule(group, kind) {
        if let Some(generate) = field("generateName").and_then(Value::as_str).filter(|g| !g.is_empty()) {
            for msg in rule(&mask_trailing_dash(generate)) {
                errs.push(format!("metadata.generateName: Invalid value: {}: {msg}", quote(generate)));
            }
        }
        if let Some(name) = field("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            for msg in rule(name) {
                errs.push(format!("metadata.name: Invalid value: {}: {msg}", quote(name)));
            }
        }
    }
    labels(field("labels"), "metadata.labels", errs);
    annotations(field("annotations"), "metadata.annotations", errs);
}

fn labels(labels: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    for (key, value) in labels.and_then(Value::as_object).into_iter().flatten() {
        for msg in is_qualified_name(key) {
            errs.push(format!("{path}: Invalid value: {}: {msg}", quote(key)));
        }
        let value = value.as_str().unwrap_or_default();
        for msg in is_label_value(value) {
            errs.push(format!("{path}: Invalid value: {}: {msg}", quote(value)));
        }
    }
}

const TOTAL_ANNOTATION_SIZE: usize = 256 * 1024;

fn annotations(annotations: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    let mut total = 0;
    for (key, value) in annotations.and_then(Value::as_object).into_iter().flatten() {
        for msg in is_qualified_name(&key.to_lowercase()) {
            errs.push(format!("{path}: Invalid value: {}: {msg}", quote(key)));
        }
        total += key.len() + value.as_str().map_or(0, str::len);
    }
    if total > TOTAL_ANNOTATION_SIZE {
        errs.push(format!("{path}: Too long: may not be more than {TOTAL_ANNOTATION_SIZE} bytes"));
    }
}

/// Deployment, ReplicaSet, StatefulSet and DaemonSet: a selector that is
/// set, not empty and matches the template's labels, then the template,
/// whose pods must restart Always.
fn workload(kind: &str, obj: &Value, errs: &mut Vec<String>) {
    let selector = obj.pointer("/spec/selector").filter(|s| !s.is_null());
    let template = obj.pointer("/spec/template");
    let template_labels = template.and_then(|t| t.pointer("/metadata/labels"));
    let empty = selector.is_some_and(|s| {
        let len = |f: &str| s.get(f).and_then(Value::as_object).map_or(0, |m| m.len())
            + s.get(f).and_then(Value::as_array).map_or(0, Vec::len);
        len("matchLabels") + len("matchExpressions") == 0
    });
    let empty_err = || {
        // ReplicaSet's message names deployments, as upstream's does.
        let owner = if kind == "ReplicaSet" { "deployment".to_string() } else { kind.to_lowercase() };
        format!("spec.selector: Invalid value: {}: empty selector is invalid for {owner}", go_label_selector(selector))
    };
    // A missing selector selects nothing, an empty one everything.
    let mismatch = match selector {
        None => true,
        Some(s) => LabelSelector::from_object(Some(s)).is_some_and(|sel| {
            !sel.is_empty() && !sel.matches(&json!({"metadata": {"labels": template_labels}}))
        }),
    };
    let mismatch_err = || format!("spec.template.metadata.labels: Invalid value: {}: `selector` does not match template `labels`", go_map(template_labels));
    if kind == "DaemonSet" {
        if mismatch {
            errs.push(mismatch_err());
        }
        if empty {
            errs.push(empty_err());
        }
    } else {
        if selector.is_none() {
            errs.push("spec.selector: Required value".to_string());
        } else if empty {
            errs.push(empty_err());
        }
        if mismatch {
            errs.push(mismatch_err());
        }
    }
    pod_template(template, "spec.template", errs);
    let policy = template.and_then(|t| t.pointer("/spec/restartPolicy")).and_then(Value::as_str).unwrap_or_default();
    if policy != "Always" {
        errs.push(format!("spec.template.spec.restartPolicy: Unsupported value: {}: supported values: \"Always\"", quote(policy)));
    }
}

/// A `*metav1.LabelSelector` as Go's `%#v` prints it.
fn go_label_selector(selector: Option<&Value>) -> String {
    let labels = selector.and_then(|s| s.get("matchLabels")).filter(|m| !m.is_null());
    let exprs = match selector.and_then(|s| s.get("matchExpressions")).and_then(Value::as_array) {
        Some(_) => "[]v1.LabelSelectorRequirement{}",
        None => "[]v1.LabelSelectorRequirement(nil)",
    };
    format!("v1.LabelSelector{{MatchLabels:{}, MatchExpressions:{exprs}}}", go_map(labels))
}

fn job_restart_policy(template: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    let policy = template.and_then(|t| t.pointer("/spec/restartPolicy")).and_then(Value::as_str).unwrap_or_default();
    if policy.is_empty() {
        errs.push(format!("{path}: Required value: valid values: \"OnFailure\", \"Never\""));
    } else if policy != "OnFailure" && policy != "Never" {
        errs.push(format!("{path}: Unsupported value: {}: supported values: \"OnFailure\", \"Never\"", quote(policy)));
    }
}

/// `ValidatePodTemplateSpec`: the template's labels and annotations, then
/// its pod spec.
fn pod_template(template: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    let meta = template.and_then(|t| t.get("metadata"));
    labels(meta.and_then(|m| m.get("labels")), &format!("{path}.metadata.labels"), errs);
    annotations(meta.and_then(|m| m.get("annotations")), &format!("{path}.metadata.annotations"), errs);
    pod_spec(template.and_then(|t| t.get("spec")), &format!("{path}.spec"), errs);
}

/// The parts of `ValidatePodSpec` clients run into most: containers (at
/// least one, unique valid names, an image, valid ports) and the restart
/// policy.
fn pod_spec(spec: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    let list = |f: &str| spec.and_then(|s| s.get(f)).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mut names = std::collections::HashSet::new();
    let containers = list("containers");
    if containers.is_empty() {
        errs.push(format!("{path}.containers: Required value"));
    }
    for (field, list) in [("containers", containers), ("initContainers", list("initContainers"))] {
        for (i, c) in list.iter().enumerate() {
            let path = format!("{path}.{field}[{i}]");
            let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                errs.push(format!("{path}.name: Required value"));
            } else {
                for msg in is_dns1123_label(name) {
                    errs.push(format!("{path}.name: Invalid value: {}: {msg}", quote(name)));
                }
            }
            if c.get("image").and_then(Value::as_str).unwrap_or_default().is_empty() {
                errs.push(format!("{path}.image: Required value"));
            }
            container_ports(c.get("ports"), &format!("{path}.ports"), errs);
            if !names.insert(name) {
                errs.push(format!("{path}.name: Duplicate value: {}", quote(name)));
            }
        }
    }
    match spec.and_then(|s| s.get("restartPolicy")).and_then(Value::as_str) {
        // Unset or empty is defaulted, or left to the Job's own check.
        None | Some("" | "Always" | "OnFailure" | "Never") => {}
        Some(other) => errs.push(format!(
            "{path}.restartPolicy: Unsupported value: {}: supported values: \"Always\", \"OnFailure\", \"Never\"",
            quote(other)
        )),
    }
}

fn port_number(port: i64, path: &str, errs: &mut Vec<String>) {
    if !(1..=65535).contains(&port) {
        errs.push(format!("{path}: Invalid value: {port}: must be between 1 and 65535, inclusive"));
    }
}

/// `IsValidPortName`.
fn port_name(name: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if name.len() > 15 {
        errs.push("must be no more than 15 characters".to_string());
    }
    if !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') {
        errs.push("must contain only alpha-numeric characters (a-z, 0-9), and hyphens (-)".to_string());
    } else if !name.bytes().any(|c| c.is_ascii_lowercase()) {
        errs.push("must contain at least one letter (a-z)".to_string());
    } else if name.contains("--") {
        errs.push("must not contain consecutive hyphens (-)".to_string());
    } else if name.starts_with('-') || name.ends_with('-') {
        errs.push("must not begin or end with a hyphen (-)".to_string());
    }
    errs
}

fn protocol(port: &Value, path: &str, errs: &mut Vec<String>) {
    match port.get("protocol").and_then(Value::as_str).unwrap_or_default() {
        "TCP" | "UDP" | "SCTP" => {}
        "" => errs.push(format!("{path}.protocol: Required value")),
        other => errs.push(format!(
            "{path}.protocol: Unsupported value: {}: supported values: \"SCTP\", \"TCP\", \"UDP\"",
            quote(other)
        )),
    }
}

fn container_ports(ports: Option<&Value>, path: &str, errs: &mut Vec<String>) {
    let mut names = std::collections::HashSet::new();
    for (i, p) in ports.and_then(Value::as_array).into_iter().flatten().enumerate() {
        let path = format!("{path}[{i}]");
        if let Some(name) = p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            let msgs = port_name(name);
            if !msgs.is_empty() {
                errs.extend(msgs.iter().map(|msg| format!("{path}.name: Invalid value: {}: {msg}", quote(name))));
            } else if !names.insert(name) {
                errs.push(format!("{path}.name: Duplicate value: {}", quote(name)));
            }
        }
        match p.get("containerPort").and_then(Value::as_i64).unwrap_or(0) {
            0 => errs.push(format!("{path}.containerPort: Required value")),
            port => port_number(port, &format!("{path}.containerPort"), errs),
        }
        if let Some(host) = p.get("hostPort").and_then(Value::as_i64).filter(|&h| h != 0) {
            port_number(host, &format!("{path}.hostPort"), errs);
        }
        protocol(p, &path, errs);
    }
}

/// `ValidateService`'s type and ports.
fn service(obj: &Value, errs: &mut Vec<String>) {
    let spec = obj.get("spec");
    let svc_type = spec.and_then(|s| s.get("type")).and_then(Value::as_str).unwrap_or("ClusterIP");
    let ports = spec.and_then(|s| s.get("ports")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let headless = spec.and_then(|s| s.get("clusterIP")).and_then(Value::as_str) == Some("None");
    if ports.is_empty() && !headless && svc_type != "ExternalName" {
        errs.push("spec.ports: Required value".to_string());
    }
    let mut names = std::collections::HashSet::new();
    for (i, p) in ports.iter().enumerate() {
        let path = format!("spec.ports[{i}]");
        let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
        if ports.len() > 1 && name.is_empty() {
            errs.push(format!("{path}.name: Required value"));
        } else if !name.is_empty() {
            for msg in is_dns1123_label(name) {
                errs.push(format!("{path}.name: Invalid value: {}: {msg}", quote(name)));
            }
            if !names.insert(name) {
                errs.push(format!("{path}.name: Duplicate value: {}", quote(name)));
            }
        }
        port_number(p.get("port").and_then(Value::as_i64).unwrap_or(0), &format!("{path}.port"), errs);
        protocol(p, &path, errs);
        match p.get("targetPort") {
            Some(Value::String(t)) => {
                for msg in port_name(t) {
                    errs.push(format!("{path}.targetPort: Invalid value: {}: {msg}", quote(t)));
                }
            }
            Some(t) => {
                if let Some(t) = t.as_i64() {
                    port_number(t, &format!("{path}.targetPort"), errs);
                }
            }
            None => {}
        }
    }
    if !matches!(svc_type, "ClusterIP" | "ExternalName" | "LoadBalancer" | "NodePort") {
        errs.push(format!(
            "spec.type: Unsupported value: {}: supported values: \"ClusterIP\", \"ExternalName\", \"LoadBalancer\", \"NodePort\"",
            quote(svc_type)
        ));
    }
}

/// How the API server joins several field errors into one message.
pub fn join(errs: &[String]) -> String {
    match errs {
        [one] => one.clone(),
        many => format!("[{}]", many.join(", ")),
    }
}

const SYSCTL_FMT: &str = r"^([a-z0-9]([-_a-z0-9]*[a-z0-9])?[\./])*[a-z0-9]([-_a-z0-9]*[a-z0-9])?$";

fn valid_sysctl_name(name: &str) -> bool {
    let segment_ok = |seg: &str| {
        let b = seg.as_bytes();
        let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
        !b.is_empty()
            && alnum(b[0])
            && alnum(b[b.len() - 1])
            && b.iter().all(|&c| alnum(c) || c == b'-' || c == b'_')
    };
    name.len() <= 253 && name.split(['.', '/']).all(segment_ok)
}

/// `resources.<kind>.<res>` of a container (not a JSON pointer: extended
/// resource names contain '/').
fn resource<'a>(c: &'a Value, kind: &str, res: &str) -> Option<&'a Value> {
    c.get("resources")?.get(kind)?.get(res)
}

/// LimitRanger: fill missing container requests and limits from the
/// namespace's `Container` LimitRanges, then enforce their min and max.
/// Requests that are still missing default to the limits, as the API server's
/// pod defaulting does.
pub fn apply_limit_ranges(pod: &mut Value, ranges: &[Value]) -> Vec<String> {
    let mut errs = Vec::new();
    let items: Vec<&Value> = ranges
        .iter()
        .flat_map(|r| r.pointer("/spec/limits").and_then(Value::as_array).into_iter().flatten())
        .filter(|l| l.get("type").and_then(Value::as_str) == Some("Container"))
        .collect();
    for field in ["initContainers", "containers"] {
        let Some(containers) = pod.pointer_mut(&format!("/spec/{field}")).and_then(Value::as_array_mut) else { continue };
        for c in containers {
            let resources = c.as_object_mut().unwrap().entry("resources").or_insert_with(|| json!({}));
            if !resources.is_object() {
                *resources = json!({});
            }
            // Pod defaulting runs before admission.
            default_requests_to_limits(resources);
            for item in &items {
                for (from, to) in [("default", "limits"), ("defaultRequest", "requests")] {
                    for (res, q) in item.get(from).and_then(Value::as_object).into_iter().flatten() {
                        let target = resources.as_object_mut().unwrap().entry(to).or_insert_with(|| json!({}));
                        if target.get(res).is_none() {
                            target[res] = q.clone();
                        }
                    }
                }
            }
            default_requests_to_limits(resources);
            for item in &items {
                for (res, min) in item.get("min").and_then(Value::as_object).into_iter().flatten() {
                    let min_v = min.as_str().and_then(quantity::parse_milli);
                    for kind in ["requests", "limits"] {
                        let v = resource(c, kind, res).and_then(Value::as_str).and_then(quantity::parse_milli);
                        if let (Some(v), Some(m)) = (v, min_v) {
                            if v < m {
                                errs.push(format!(
                                    "minimum {res} usage per Container is {}, but {} is {}",
                                    min.as_str().unwrap_or_default(),
                                    if kind == "requests" { "request" } else { "limit" },
                                    resource(c, kind, res).and_then(Value::as_str).unwrap_or_default()
                                ));
                            }
                        }
                    }
                }
                for (res, max) in item.get("max").and_then(Value::as_object).into_iter().flatten() {
                    let max_v = max.as_str().and_then(quantity::parse_milli);
                    let limit = resource(c, "limits", res).and_then(Value::as_str).and_then(quantity::parse_milli);
                    match (limit, max_v) {
                        (None, Some(_)) => errs.push(format!("maximum {res} usage per Container is {}. No limit is specified", max.as_str().unwrap_or_default())),
                        (Some(l), Some(m)) if l > m => errs.push(format!(
                            "maximum {res} usage per Container is {}, but limit is {}",
                            max.as_str().unwrap_or_default(),
                            resource(c, "limits", res).and_then(Value::as_str).unwrap_or_default()
                        )),
                        _ => {}
                    }
                }
            }
        }
    }
    errs
}

/// `requests[r]` defaults to `limits[r]` when only the limit is set.
pub fn default_requests_to_limits(resources: &mut Value) {
    let Some(limits) = resources.get("limits").and_then(Value::as_object).cloned() else { return };
    if limits.is_empty() {
        return;
    }
    let requests = resources.as_object_mut().unwrap().entry("requests").or_insert_with(|| json!({}));
    for (res, q) in limits {
        if requests.get(&res).is_none() {
            requests[res] = q;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysctl_names() {
        assert!(valid_sysctl_name("kernel.shmmax"));
        assert!(valid_sysctl_name("net/ipv4/ip_forward"));
        assert!(valid_sysctl_name("safe-and-unsafe"));
        assert!(!valid_sysctl_name("foo-"));
        assert!(!valid_sysctl_name("bar.."));
    }

    #[test]
    fn names_by_kind() {
        let cm = json!({"metadata": {"name": "Bad_Name"}});
        assert_eq!(
            validate("", "ConfigMap", &cm, None),
            vec!["metadata.name: Invalid value: \"Bad_Name\": a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must start and end with an alphanumeric character (e.g. 'example.com', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*')"]
        );
        assert!(validate("", "ConfigMap", &json!({"metadata": {"name": "a.b-c"}}), None).is_empty());
        let svc = json!({"metadata": {"name": "a.b"}, "spec": {"ports": [{"port": 80, "protocol": "TCP"}]}});
        assert!(validate("", "Service", &svc, None)[0].contains("a DNS-1035 label"));
        assert_eq!(validate("", "Namespace", &json!({"metadata": {"name": "a.b"}}), None), vec!["metadata.name: Invalid value: \"a.b\": must not contain dots"]);
        assert!(validate("", "Namespace", &json!({"metadata": {"name": "A"}}), None)[0].contains("RFC 1123 label"));
        // RBAC names are path segments.
        assert!(validate("rbac.authorization.k8s.io", "ClusterRole", &json!({"metadata": {"name": "system:x"}}), None).is_empty());
        // A generateName may end in a dash.
        let generated = json!({"metadata": {"name": "web-x1y2z", "generateName": "web-"}});
        assert!(validate("", "ConfigMap", &generated, None).is_empty());
        let generated = json!({"metadata": {"name": "Web-x1y2z", "generateName": "Web-"}});
        assert_eq!(validate("", "ConfigMap", &generated, None).len(), 2);
    }

    #[test]
    fn labels_and_annotations() {
        let cm = json!({"metadata": {"name": "a", "labels": {"bad key": "ok", "good": "-x", "example.com/ok": ""}, "annotations": {"A_b/c": "x"}}});
        let errs = validate("", "ConfigMap", &cm, None);
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs[0].starts_with("metadata.labels: Invalid value: \"bad key\": name part must consist"));
        assert!(errs[1].starts_with("metadata.labels: Invalid value: \"-x\": a valid label must be an empty string"));
        assert!(errs[2].starts_with("metadata.annotations: Invalid value: \"A_b/c\": prefix part a lowercase RFC 1123 subdomain"));
        let big = json!({"metadata": {"name": "a", "annotations": {"k": "x".repeat(TOTAL_ANNOTATION_SIZE)}}});
        assert_eq!(validate("", "ConfigMap", &big, None), vec!["metadata.annotations: Too long: may not be more than 262144 bytes"]);
    }

    fn deployment(selector: Value) -> Value {
        json!({"metadata": {"name": "web"}, "spec": {"selector": selector, "template": {
            "metadata": {"labels": {"app": "web"}},
            "spec": {"restartPolicy": "Always", "containers": [{"name": "web", "image": "nginx"}]},
        }}})
    }

    #[test]
    fn workload_selectors() {
        assert!(validate("apps", "Deployment", &deployment(json!({"matchLabels": {"app": "web"}})), None).is_empty());
        assert_eq!(
            validate("apps", "Deployment", &deployment(Value::Null), None),
            vec![
                "spec.selector: Required value",
                "spec.template.metadata.labels: Invalid value: map[string]string{\"app\":\"web\"}: `selector` does not match template `labels`",
            ]
        );
        assert_eq!(
            validate("apps", "ReplicaSet", &deployment(json!({"matchLabels": {}})), None),
            vec!["spec.selector: Invalid value: v1.LabelSelector{MatchLabels:map[string]string{}, MatchExpressions:[]v1.LabelSelectorRequirement(nil)}: empty selector is invalid for deployment"]
        );
        let other = deployment(json!({"matchExpressions": [{"key": "app", "operator": "In", "values": ["db"]}]}));
        assert_eq!(validate("apps", "StatefulSet", &other, None).len(), 1);
        let mut never = deployment(json!({"matchLabels": {"app": "web"}}));
        never["spec"]["template"]["spec"]["restartPolicy"] = json!("Never");
        assert_eq!(
            validate("apps", "DaemonSet", &never, None),
            vec!["spec.template.spec.restartPolicy: Unsupported value: \"Never\": supported values: \"Always\""]
        );
    }

    #[test]
    fn pod_containers_and_ports() {
        let pod = |containers: Value| json!({"metadata": {"name": "p"}, "spec": {"containers": containers}});
        assert_eq!(validate("", "Pod", &pod(json!([])), None), vec!["spec.containers: Required value"]);
        let errs = validate(
            "",
            "Pod",
            &pod(json!([
                {"name": "a", "image": "nginx", "ports": [{"containerPort": 70000, "protocol": "TCP"}, {"name": "Http", "containerPort": 80, "protocol": "HTTP"}]},
                {"name": "a"},
                {"name": "B_c", "image": "x"},
            ])),
            None,
        );
        assert_eq!(
            errs,
            vec![
                "spec.containers[0].ports[0].containerPort: Invalid value: 70000: must be between 1 and 65535, inclusive",
                "spec.containers[0].ports[1].name: Invalid value: \"Http\": must contain only alpha-numeric characters (a-z, 0-9), and hyphens (-)",
                "spec.containers[0].ports[1].protocol: Unsupported value: \"HTTP\": supported values: \"SCTP\", \"TCP\", \"UDP\"",
                "spec.containers[1].image: Required value",
                "spec.containers[1].name: Duplicate value: \"a\"",
                "spec.containers[2].name: Invalid value: \"B_c\": a lowercase RFC 1123 label must consist of lower case alphanumeric characters or '-', and must start and end with an alphanumeric character (e.g. 'my-name',  or '123-abc', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?')",
            ]
        );
        let mut init = pod(json!([{"name": "a", "image": "x"}]));
        init["spec"]["initContainers"] = json!([{"name": "a", "image": "x"}]);
        assert_eq!(validate("", "Pod", &init, None), vec!["spec.initContainers[0].name: Duplicate value: \"a\""]);
    }

    #[test]
    fn service_ports() {
        let svc = |spec: Value| json!({"metadata": {"name": "web"}, "spec": spec});
        assert!(validate("", "Service", &svc(json!({"ports": [{"port": 80, "targetPort": "http", "protocol": "TCP"}]})), None).is_empty());
        assert_eq!(validate("", "Service", &svc(json!({"type": "ClusterIP"})), None), vec!["spec.ports: Required value"]);
        assert!(validate("", "Service", &svc(json!({"clusterIP": "None"})), None).is_empty());
        assert_eq!(
            validate("", "Service", &svc(json!({"ports": [{"port": 70000, "targetPort": 70000, "protocol": "TCP"}]})), None),
            vec![
                "spec.ports[0].port: Invalid value: 70000: must be between 1 and 65535, inclusive",
                "spec.ports[0].targetPort: Invalid value: 70000: must be between 1 and 65535, inclusive",
            ]
        );
        assert_eq!(
            validate("", "Service", &svc(json!({"type": "Foo", "ports": [{"name": "a", "port": 80, "protocol": "TCP"}, {"port": 81, "protocol": "TCP"}]})), None),
            vec![
                "spec.ports[1].name: Required value",
                "spec.type: Unsupported value: \"Foo\": supported values: \"ClusterIP\", \"ExternalName\", \"LoadBalancer\", \"NodePort\"",
            ]
        );
    }

    #[test]
    fn job_restart_policies() {
        let job = |policy: &str| json!({"metadata": {"name": "j"}, "spec": {"template": {"spec": {"restartPolicy": policy, "containers": [{"name": "a", "image": "x"}]}}}});
        assert!(validate("batch", "Job", &job("Never"), None).is_empty());
        assert_eq!(
            validate("batch", "Job", &job("Always"), None),
            vec!["spec.template.spec.restartPolicy: Unsupported value: \"Always\": supported values: \"OnFailure\", \"Never\""]
        );
        let mut unset = job("");
        unset["spec"]["template"]["spec"].as_object_mut().unwrap().remove("restartPolicy");
        assert_eq!(
            validate("batch", "Job", &unset, None),
            vec!["spec.template.spec.restartPolicy: Required value: valid values: \"OnFailure\", \"Never\""]
        );
        let cron = json!({"metadata": {"name": "c"}, "spec": {"jobTemplate": job("Always")}});
        assert_eq!(validate("batch", "CronJob", &cron, None).len(), 1);
    }

    #[test]
    fn limit_range_defaults_and_bounds() {
        let lr = json!({"spec": {"limits": [{
            "type": "Container",
            "min": {"cpu": "50m", "memory": "100Mi"},
            "max": {"cpu": "500m", "memory": "500Mi"},
            "default": {"cpu": "500m", "memory": "500Mi"},
            "defaultRequest": {"cpu": "100m", "memory": "200Mi"},
        }]}});
        let mut pod = json!({"spec": {"containers": [{"name": "a", "resources": {"requests": {"memory": "150Mi"}, "limits": {"cpu": "300m"}}}]}});
        assert!(apply_limit_ranges(&mut pod, &[lr.clone()]).is_empty());
        assert_eq!(
            pod["spec"]["containers"][0]["resources"],
            json!({"requests": {"cpu": "300m", "memory": "150Mi"}, "limits": {"cpu": "300m", "memory": "500Mi"}})
        );
        let mut big = json!({"spec": {"containers": [{"name": "a", "resources": {"limits": {"cpu": "600m"}}}]}});
        assert!(!apply_limit_ranges(&mut big, &[lr.clone()]).is_empty());
        // A request above the defaulted limit fails pod validation instead.
        let mut over = json!({"spec": {"containers": [{"name": "a", "resources": {"requests": {"cpu": "600m"}}}]}});
        assert!(apply_limit_ranges(&mut over, &[lr]).is_empty());
        assert!(!validate("", "Pod", &over, None).is_empty());
    }
}
