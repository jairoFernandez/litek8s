//! Per-kind validation and the built-in admission plugins the conformance
//! suite relies on (LimitRanger, RuntimeClass). Errors use the field-path
//! format of the real API server (`spec.x: Invalid value: "y": reason`) so
//! clients matching on the message behave the same.

use serde_json::{Value, json};

use super::quantity;

/// Field errors for a create (`old` is `None`) or an update of `obj`.
pub fn validate(group: &str, kind: &str, obj: &Value, old: Option<&Value>) -> Vec<String> {
    let mut errs = Vec::new();
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
                        let v = c.pointer(&format!("/resources/{kind}/{res}")).and_then(Value::as_str).and_then(quantity::parse_milli);
                        if let (Some(v), Some(m)) = (v, min_v) {
                            if v < m {
                                errs.push(format!(
                                    "minimum {res} usage per Container is {}, but {} is {}",
                                    min.as_str().unwrap_or_default(),
                                    if kind == "requests" { "request" } else { "limit" },
                                    c.pointer(&format!("/resources/{kind}/{res}")).and_then(Value::as_str).unwrap_or_default()
                                ));
                            }
                        }
                    }
                }
                for (res, max) in item.get("max").and_then(Value::as_object).into_iter().flatten() {
                    let max_v = max.as_str().and_then(quantity::parse_milli);
                    let limit = c.pointer(&format!("/resources/limits/{res}")).and_then(Value::as_str).and_then(quantity::parse_milli);
                    match (limit, max_v) {
                        (None, Some(_)) => errs.push(format!("maximum {res} usage per Container is {}. No limit is specified", max.as_str().unwrap_or_default())),
                        (Some(l), Some(m)) if l > m => errs.push(format!(
                            "maximum {res} usage per Container is {}, but limit is {}",
                            max.as_str().unwrap_or_default(),
                            c.pointer(&format!("/resources/limits/{res}")).and_then(Value::as_str).unwrap_or_default()
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
