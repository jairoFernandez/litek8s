//! Pod Security admission, and the `untrusted` isolation of a namespace.
//!
//! A Namespace's `pod-security.kubernetes.io/enforce` label (`privileged`,
//! `baseline`, `restricted`) is enforced on its pods as upstream's built-in
//! admission does: the checks of the Pod Security Standards, with upstream's
//! messages. `warn` and `audit` are accepted and ignored.
//!
//! `litek8s.dev/isolation: untrusted` on a Namespace declares that its pods
//! run code nobody vouches for. The pod's author then has nothing to
//! remember: [`harden`] fills in what `restricted` asks for (and no
//! ServiceAccount token, no service links, resource limits, a PID limit),
//! the result must pass `restricted`, and the pod carries
//! [`ISOLATION`]`: untrusted` for the node agent (PID limit) and the network
//! rules (`crate::kube::netpol`). Everything here is pure.

use serde_json::{Map, Value, json};

/// The Namespace label and, on its pods, the annotation the server sets.
pub const ISOLATION: &str = "litek8s.dev/isolation";
pub const UNTRUSTED: &str = "untrusted";
/// Processes and threads a container may have: an annotation of the
/// Namespace (its pods' default) and of the pod (what the agent applies).
pub const PIDS_LIMIT: &str = "litek8s.dev/pids-limit";
pub const DEFAULT_PIDS_LIMIT: i64 = 256;
const ENFORCE: &str = "pod-security.kubernetes.io/enforce";

/// What a container of an untrusted namespace gets when neither it nor a
/// LimitRange says.
const DEFAULT_LIMITS: [(&str, &str); 2] = [("cpu", "1"), ("memory", "512Mi")];
const DEFAULT_REQUESTS: [(&str, &str); 2] = [("cpu", "50m"), ("memory", "64Mi")];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Privileged,
    Baseline,
    Restricted,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "privileged" => Some(Self::Privileged),
            "baseline" => Some(Self::Baseline),
            "restricted" => Some(Self::Restricted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Privileged => "privileged",
            Self::Baseline => "baseline",
            Self::Restricted => "restricted",
        }
    }
}

fn label<'a>(namespace: Option<&'a Value>, key: &str) -> Option<&'a str> {
    namespace?.get("metadata")?.get("labels")?.get(key)?.as_str()
}

/// Whether the Namespace is declared untrusted.
pub fn untrusted(namespace: Option<&Value>) -> bool {
    label(namespace, ISOLATION) == Some(UNTRUSTED)
}

/// The level enforced on the Namespace's pods: its label, and at least
/// `restricted` when it is untrusted.
pub fn enforced(namespace: Option<&Value>) -> Level {
    if untrusted(namespace) {
        return Level::Restricted;
    }
    label(namespace, ENFORCE).and_then(Level::parse).unwrap_or(Level::Privileged)
}

/// The PID limit of an untrusted namespace's containers.
pub fn pids_limit(namespace: Option<&Value>) -> i64 {
    namespace
        .and_then(|n| n.pointer("/metadata/annotations"))
        .and_then(|a| a.get(PIDS_LIMIT))
        .and_then(Value::as_str)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_PIDS_LIMIT)
}

/// A label value an invalid Namespace would carry: `None` when fine.
pub fn invalid_namespace_label(namespace: &Value) -> Option<String> {
    for key in [ENFORCE, "pod-security.kubernetes.io/warn", "pod-security.kubernetes.io/audit"] {
        if let Some(v) = label(Some(namespace), key).filter(|v| Level::parse(v).is_none()) {
            return Some(format!("metadata.labels[{key}]: Invalid value: \"{v}\": must be one of privileged, baseline, restricted"));
        }
    }
    label(Some(namespace), ISOLATION).filter(|v| *v != UNTRUSTED).map(|v| format!("metadata.labels[{ISOLATION}]: Invalid value: \"{v}\": must be {UNTRUSTED}"))
}

fn entry<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let v = map.entry(key.to_string()).or_insert_with(|| json!({}));
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().unwrap()
}

fn harden_container(c: &mut Value) {
    let Some(c) = c.as_object_mut() else { return };
    let sc = entry(c, "securityContext");
    sc.entry("allowPrivilegeEscalation").or_insert(json!(false));
    let caps = entry(sc, "capabilities");
    if !caps.get("drop").is_some_and(|d| d.as_array().is_some_and(|d| !d.is_empty())) {
        caps.insert("drop".into(), json!(["ALL"]));
    }
}

/// A new pod of an untrusted namespace, before the API server's own
/// defaults: what its author left unsaid becomes the safe choice. What they
/// said and is unsafe stays, for [`violations`] to refuse.
pub fn harden(pod: &mut Value, namespace: Option<&Value>) {
    let limit = pids_limit(namespace);
    let Some(root) = pod.as_object_mut() else { return };
    let annotations = entry(entry(root, "metadata"), "annotations");
    annotations.insert(ISOLATION.into(), json!(UNTRUSTED));
    annotations.insert(PIDS_LIMIT.into(), json!(limit.to_string()));
    let spec = entry(root, "spec");
    spec.entry("automountServiceAccountToken").or_insert(json!(false));
    spec.entry("enableServiceLinks").or_insert(json!(false));
    let sc = entry(spec, "securityContext");
    sc.entry("runAsNonRoot").or_insert(json!(true));
    sc.entry("seccompProfile").or_insert(json!({"type": "RuntimeDefault"}));
    sc.entry("appArmorProfile").or_insert(json!({"type": "RuntimeDefault"}));
    for field in ["initContainers", "containers", "ephemeralContainers"] {
        for c in spec.get_mut(field).and_then(Value::as_array_mut).into_iter().flatten() {
            harden_container(c);
        }
    }
}

/// Ephemeral containers added to a pod of an untrusted namespace.
pub fn harden_ephemeral(containers: &mut Value) {
    for c in containers.as_array_mut().into_iter().flatten() {
        harden_container(c);
    }
}

/// Limits and requests for the containers that still have none after the
/// namespace's LimitRanges (untrusted namespaces).
pub fn default_resources(pod: &mut Value) {
    let Some(spec) = pod.get_mut("spec").and_then(Value::as_object_mut) else { return };
    for field in ["initContainers", "containers"] {
        for c in spec.get_mut(field).and_then(Value::as_array_mut).into_iter().flatten() {
            let Some(c) = c.as_object_mut() else { continue };
            let resources = entry(c, "resources");
            let given: Map<String, Value> = resources.get("limits").and_then(Value::as_object).cloned().unwrap_or_default();
            let limits = entry(resources, "limits");
            for (res, value) in DEFAULT_LIMITS {
                limits.entry(res).or_insert(json!(value));
            }
            let requests = entry(resources, "requests");
            for (res, value) in DEFAULT_REQUESTS {
                // As the API server does: a limit the author gave is the request.
                requests.entry(res).or_insert_with(|| given.get(res).cloned().unwrap_or_else(|| json!(value)));
            }
        }
    }
}

/// The annotations the server owns on a pod: an update keeps what the pod
/// was created with.
pub fn keep_annotations(new: &mut Value, old: &Value) {
    let old_annotations = old.pointer("/metadata/annotations");
    let was_untrusted = old_annotations.and_then(|a| a.get(ISOLATION)).and_then(Value::as_str) == Some(UNTRUSTED);
    for key in [ISOLATION, PIDS_LIMIT] {
        let before = old_annotations.and_then(|a| a.get(key)).cloned();
        if !was_untrusted && key == PIDS_LIMIT {
            continue;
        }
        let Some(root) = new.as_object_mut() else { return };
        let annotations = entry(entry(root, "metadata"), "annotations");
        match before {
            Some(v) => {
                annotations.insert(key.into(), v);
            }
            None => {
                annotations.remove(key);
            }
        }
    }
}

const BASELINE_CAPS: &[&str] = &[
    "AUDIT_WRITE",
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "MKNOD",
    "NET_BIND_SERVICE",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_CHROOT",
];

const SAFE_SYSCTLS: &[&str] = &[
    "kernel.shm_rmid_forced",
    "net.ipv4.ip_local_port_range",
    "net.ipv4.ip_unprivileged_port_start",
    "net.ipv4.tcp_syncookies",
    "net.ipv4.ping_group_range",
    "net.ipv4.ip_local_reserved_ports",
    "net.ipv4.tcp_keepalive_time",
    "net.ipv4.tcp_fin_timeout",
    "net.ipv4.tcp_keepalive_intvl",
    "net.ipv4.tcp_keepalive_probes",
];

const RESTRICTED_VOLUMES: &[&str] = &["configMap", "csi", "downwardAPI", "emptyDir", "ephemeral", "persistentVolumeClaim", "projected", "secret"];

const SELINUX_TYPES: &[&str] = &["", "container_t", "container_init_t", "container_kvm_t", "container_engine_t"];

/// `container "a"` / `containers "a", "b"`, as upstream words them.
fn containers_phrase(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    format!("{} {}", if names.len() == 1 { "container" } else { "containers" }, quoted.join(", "))
}

fn quoted_list(items: &[String]) -> String {
    items.iter().map(|i| format!("\"{i}\"")).collect::<Vec<_>>().join(", ")
}

/// Why `pod` (an object with `metadata` and `spec`) does not meet `level`:
/// one entry per failed check; empty when it does.
pub fn violations(level: Level, pod: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if level == Level::Privileged {
        return out;
    }
    let empty = json!({});
    let spec = pod.get("spec").unwrap_or(&empty);
    let pod_sc = spec.get("securityContext").filter(|s| s.is_object()).unwrap_or(&empty);
    let containers: Vec<&Value> = ["initContainers", "containers", "ephemeralContainers"]
        .iter()
        .flat_map(|f| spec.get(*f).and_then(Value::as_array).into_iter().flatten())
        .collect();
    let name = |c: &Value| c.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let sc = |c: &Value| c.get("securityContext").filter(|s| s.is_object()).cloned().unwrap_or_else(|| json!({}));
    // The containers a predicate holds for.
    let failing = |bad: &dyn Fn(&Value, &Value) -> bool| -> Vec<String> { containers.iter().filter(|c| bad(c, &sc(c))).map(|c| name(c)).collect() };

    // ---- baseline ----
    let host: Vec<String> = ["hostNetwork", "hostPID", "hostIPC"].iter().filter(|f| spec.get(**f) == Some(&json!(true))).map(|f| format!("{f}=true")).collect();
    if !host.is_empty() {
        out.push(format!("host namespaces ({})", host.join(", ")));
    }
    let privileged = failing(&|_, sc| sc.get("privileged") == Some(&json!(true)));
    if !privileged.is_empty() {
        out.push(format!("privileged ({} must not set securityContext.privileged=true)", containers_phrase(&privileged)));
    }
    let added = |sc: &Value| -> Vec<String> {
        sc.pointer("/capabilities/add").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|c| c.trim_start_matches("CAP_").to_uppercase()).collect()
    };
    if level == Level::Baseline {
        for c in &containers {
            let extra: Vec<String> = added(&sc(c)).into_iter().filter(|a| !BASELINE_CAPS.contains(&a.as_str())).collect();
            if !extra.is_empty() {
                out.push(format!("non-default capabilities (container \"{}\" must not include {} in securityContext.capabilities.add)", name(c), quoted_list(&extra)));
            }
        }
    }
    let volumes: Vec<&Value> = spec.get("volumes").and_then(Value::as_array).into_iter().flatten().collect();
    let volume_name = |v: &Value| v.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let host_paths: Vec<String> = volumes.iter().filter(|v| v.get("hostPath").is_some_and(|h| !h.is_null())).map(|v| volume_name(v)).collect();
    if !host_paths.is_empty() {
        out.push(format!("hostPath volumes ({} {})", if host_paths.len() == 1 { "volume" } else { "volumes" }, quoted_list(&host_paths)));
    }
    for c in &containers {
        let ports: Vec<String> = c
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.get("hostPort").and_then(Value::as_i64))
            .filter(|p| *p != 0)
            .map(|p| p.to_string())
            .collect();
        if !ports.is_empty() {
            out.push(format!("hostPort (container \"{}\" uses hostPort{} {})", name(c), if ports.len() == 1 { "" } else { "s" }, ports.join(", ")));
        }
    }
    let unconfined_apparmor = |sc: &Value| sc.pointer("/appArmorProfile/type").and_then(Value::as_str) == Some("Unconfined");
    let apparmor_annotations = pod
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .any(|(k, v)| k.starts_with("container.apparmor.security.beta.kubernetes.io/") && v.as_str().is_some_and(|v| v != "runtime/default" && !v.starts_with("localhost/")));
    if unconfined_apparmor(pod_sc) || containers.iter().any(|c| unconfined_apparmor(&sc(c))) || apparmor_annotations {
        out.push("forbidden AppArmor profile (pod or containers must not set appArmorProfile.type to \"Unconfined\")".into());
    }
    let selinux_bad = |sc: &Value| {
        let Some(o) = sc.get("seLinuxOptions").filter(|o| o.is_object()) else { return false };
        let s = |f: &str| o.get(f).and_then(Value::as_str).unwrap_or_default();
        !SELINUX_TYPES.contains(&s("type")) || !s("user").is_empty() || !s("role").is_empty()
    };
    if selinux_bad(pod_sc) || containers.iter().any(|c| selinux_bad(&sc(c))) {
        out.push("seLinuxOptions (pod or containers set forbidden securityContext.seLinuxOptions)".into());
    }
    let proc_mount = failing(&|_, sc| sc.get("procMount").and_then(Value::as_str).is_some_and(|p| p != "Default"));
    if !proc_mount.is_empty() {
        out.push(format!("procMount ({} must not set securityContext.procMount to \"Unmasked\")", containers_phrase(&proc_mount)));
    }
    let seccomp = |sc: &Value| sc.pointer("/seccompProfile/type").and_then(Value::as_str).map(str::to_string);
    let pod_seccomp = seccomp(pod_sc);
    if level == Level::Baseline {
        let unconfined = failing(&|_, sc| seccomp(sc).as_deref() == Some("Unconfined"));
        if pod_seccomp.as_deref() == Some("Unconfined") || !unconfined.is_empty() {
            let who = if unconfined.is_empty() { "pod".to_string() } else { containers_phrase(&unconfined) };
            out.push(format!("seccompProfile ({who} must not set securityContext.seccompProfile.type to \"Unconfined\")"));
        }
    }
    let sysctls: Vec<String> = pod_sc
        .get("sysctls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .filter(|n| !SAFE_SYSCTLS.contains(n))
        .map(str::to_string)
        .collect();
    if !sysctls.is_empty() {
        out.push(format!("forbidden sysctls ({})", sysctls.join(", ")));
    }
    let host_process = |sc: &Value| sc.pointer("/windowsOptions/hostProcess") == Some(&json!(true));
    if host_process(pod_sc) || containers.iter().any(|c| host_process(&sc(c))) {
        out.push("hostProcess (pod or containers must not set securityContext.windowsOptions.hostProcess=true)".into());
    }
    if level == Level::Baseline {
        return out;
    }

    // ---- restricted ----
    for v in &volumes {
        let kinds: Vec<&String> = v.as_object().into_iter().flat_map(|m| m.iter()).filter(|(k, val)| *k != "name" && !val.is_null()).map(|(k, _)| k).collect();
        if let Some(kind) = kinds.iter().find(|k| !RESTRICTED_VOLUMES.contains(&k.as_str()) && k.as_str() != "hostPath") {
            out.push(format!("restricted volume types (volume \"{}\" uses restricted volume type \"{kind}\")", volume_name(v)));
        }
    }
    let escalation = failing(&|_, sc| sc.get("allowPrivilegeEscalation") != Some(&json!(false)));
    if !escalation.is_empty() {
        out.push(format!("allowPrivilegeEscalation != false ({} must set securityContext.allowPrivilegeEscalation=false)", containers_phrase(&escalation)));
    }
    let no_drop = failing(&|_, sc| !sc.pointer("/capabilities/drop").and_then(Value::as_array).is_some_and(|d| d.iter().any(|c| c.as_str().is_some_and(|c| c.eq_ignore_ascii_case("ALL")))));
    if !no_drop.is_empty() {
        out.push(format!("unrestricted capabilities ({} must set securityContext.capabilities.drop=[\"ALL\"])", containers_phrase(&no_drop)));
    }
    for c in &containers {
        let extra: Vec<String> = added(&sc(c)).into_iter().filter(|a| a != "NET_BIND_SERVICE").collect();
        if !extra.is_empty() {
            out.push(format!("unrestricted capabilities (container \"{}\" must not include {} in securityContext.capabilities.add)", name(c), quoted_list(&extra)));
        }
    }
    let pod_non_root = pod_sc.get("runAsNonRoot").and_then(Value::as_bool);
    let explicit_root = failing(&|_, sc| sc.get("runAsNonRoot") == Some(&json!(false)));
    let unset_non_root = failing(&|_, sc| sc.get("runAsNonRoot").and_then(Value::as_bool).is_none());
    if !explicit_root.is_empty() {
        out.push(format!("runAsNonRoot != true ({} must not set securityContext.runAsNonRoot=false)", containers_phrase(&explicit_root)));
    } else if pod_non_root != Some(true) && !unset_non_root.is_empty() {
        out.push(format!("runAsNonRoot != true (pod or {} must set securityContext.runAsNonRoot=true)", containers_phrase(&unset_non_root)));
    }
    let root_user = failing(&|_, sc| sc.get("runAsUser").and_then(Value::as_i64) == Some(0));
    if pod_sc.get("runAsUser").and_then(Value::as_i64) == Some(0) || !root_user.is_empty() {
        let who = if root_user.is_empty() { "pod".to_string() } else { containers_phrase(&root_user) };
        out.push(format!("runAsUser=0 ({who} must not set runAsUser=0)"));
    }
    let allowed = |t: Option<&str>| matches!(t, Some("RuntimeDefault" | "Localhost"));
    let bad_seccomp = failing(&|_, sc| match seccomp(sc) {
        Some(t) => !allowed(Some(&t)),
        None => !allowed(pod_seccomp.as_deref()),
    });
    if !bad_seccomp.is_empty() || (containers.is_empty() && !allowed(pod_seccomp.as_deref())) {
        out.push(format!("seccompProfile (pod or {} must set securityContext.seccompProfile.type to \"RuntimeDefault\" or \"Localhost\")", containers_phrase(&bad_seccomp)));
    }
    out
}

/// The admission error of a pod that does not meet `level`.
pub fn message(level: Level, violations: &[String]) -> String {
    format!("violates PodSecurity \"{}:latest\": {}", level.as_str(), violations.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(labels: Value) -> Value {
        json!({"metadata": {"name": "runs", "labels": labels}})
    }

    fn bare() -> Value {
        json!({"metadata": {"name": "p"}, "spec": {"containers": [{"name": "c", "image": "busybox"}]}})
    }

    #[test]
    fn levels_from_namespace_labels() {
        assert_eq!(enforced(None), Level::Privileged);
        assert_eq!(enforced(Some(&ns(json!({})))), Level::Privileged);
        assert_eq!(enforced(Some(&ns(json!({ENFORCE: "baseline"})))), Level::Baseline);
        assert_eq!(enforced(Some(&ns(json!({ENFORCE: "restricted"})))), Level::Restricted);
        // Untrusted is restricted, whatever the standard label says.
        let n = ns(json!({ISOLATION: "untrusted", ENFORCE: "privileged"}));
        assert!(untrusted(Some(&n)));
        assert_eq!(enforced(Some(&n)), Level::Restricted);
        assert!(invalid_namespace_label(&n).is_none());
        assert!(invalid_namespace_label(&ns(json!({ENFORCE: "strict"}))).unwrap().contains("must be one of"));
        assert!(invalid_namespace_label(&ns(json!({ISOLATION: "yes"}))).is_some());
    }

    #[test]
    fn privileged_and_host_pods_fail_baseline() {
        let pod = json!({"metadata": {"name": "p"}, "spec": {
            "hostNetwork": true, "hostPID": true,
            "volumes": [{"name": "root", "hostPath": {"path": "/"}}],
            "securityContext": {"sysctls": [{"name": "kernel.msgmax", "value": "1"}, {"name": "net.ipv4.ping_group_range", "value": "0 1"}]},
            "containers": [{"name": "c", "securityContext": {"privileged": true, "capabilities": {"add": ["NET_ADMIN", "CHOWN"]}, "procMount": "Unmasked", "seccompProfile": {"type": "Unconfined"}},
                "ports": [{"containerPort": 80, "hostPort": 80}]}],
        }});
        assert!(violations(Level::Privileged, &pod).is_empty());
        let v = violations(Level::Baseline, &pod);
        assert_eq!(
            v,
            [
                "host namespaces (hostNetwork=true, hostPID=true)",
                "privileged (container \"c\" must not set securityContext.privileged=true)",
                "non-default capabilities (container \"c\" must not include \"NET_ADMIN\" in securityContext.capabilities.add)",
                "hostPath volumes (volume \"root\")",
                "hostPort (container \"c\" uses hostPort 80)",
                "procMount (container \"c\" must not set securityContext.procMount to \"Unmasked\")",
                "seccompProfile (container \"c\" must not set securityContext.seccompProfile.type to \"Unconfined\")",
                "forbidden sysctls (kernel.msgmax)",
            ]
        );
        assert_eq!(message(Level::Baseline, &v[..1]), "violates PodSecurity \"baseline:latest\": host namespaces (hostNetwork=true, hostPID=true)");
        // A plain pod is fine at baseline, not at restricted.
        assert!(violations(Level::Baseline, &bare()).is_empty());
    }

    #[test]
    fn restricted_wants_everything_said() {
        assert_eq!(
            violations(Level::Restricted, &bare()),
            [
                "allowPrivilegeEscalation != false (container \"c\" must set securityContext.allowPrivilegeEscalation=false)",
                "unrestricted capabilities (container \"c\" must set securityContext.capabilities.drop=[\"ALL\"])",
                "runAsNonRoot != true (pod or container \"c\" must set securityContext.runAsNonRoot=true)",
                "seccompProfile (pod or container \"c\" must set securityContext.seccompProfile.type to \"RuntimeDefault\" or \"Localhost\")",
            ]
        );
        let ok = json!({"metadata": {"name": "p"}, "spec": {
            "securityContext": {"runAsNonRoot": true, "seccompProfile": {"type": "RuntimeDefault"}},
            "volumes": [{"name": "job", "secret": {"secretName": "s"}}, {"name": "tmp", "emptyDir": {}}],
            "initContainers": [{"name": "i", "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}}}],
            "containers": [{"name": "c", "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["all"], "add": ["NET_BIND_SERVICE"]}}}],
        }});
        assert!(violations(Level::Restricted, &ok).is_empty());
        let mut bad = ok.clone();
        bad["spec"]["volumes"][1] = json!({"name": "tmp", "nfs": {"server": "x", "path": "/"}});
        bad["spec"]["containers"][0]["securityContext"]["capabilities"]["add"] = json!(["NET_RAW"]);
        bad["spec"]["containers"][0]["securityContext"]["runAsUser"] = json!(0);
        bad["spec"]["initContainers"][0]["securityContext"]["runAsNonRoot"] = json!(false);
        assert_eq!(
            violations(Level::Restricted, &bad),
            [
                "restricted volume types (volume \"tmp\" uses restricted volume type \"nfs\")",
                "unrestricted capabilities (container \"c\" must not include \"NET_RAW\" in securityContext.capabilities.add)",
                "runAsNonRoot != true (container \"i\" must not set securityContext.runAsNonRoot=false)",
                "runAsUser=0 (container \"c\" must not set runAsUser=0)",
            ]
        );
    }

    #[test]
    fn harden_makes_a_bare_pod_restricted() {
        let n = ns(json!({ISOLATION: "untrusted"}));
        let mut pod = bare();
        pod["spec"]["initContainers"] = json!([{"name": "i", "image": "busybox"}]);
        harden(&mut pod, Some(&n));
        assert!(violations(Level::Restricted, &pod).is_empty(), "{:?}", violations(Level::Restricted, &pod));
        assert_eq!(pod["spec"]["automountServiceAccountToken"], false);
        assert_eq!(pod["spec"]["enableServiceLinks"], false);
        assert_eq!(pod["metadata"]["annotations"][ISOLATION], "untrusted");
        assert_eq!(pod["metadata"]["annotations"][PIDS_LIMIT], "256");
        assert_eq!(pod["spec"]["containers"][0]["securityContext"], json!({"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}}));
    }

    #[test]
    fn harden_keeps_what_the_author_said_and_the_check_refuses_it() {
        let mut n = ns(json!({ISOLATION: "untrusted"}));
        n["metadata"]["annotations"] = json!({PIDS_LIMIT: "64"});
        let mut pod = json!({"metadata": {"name": "p", "annotations": {PIDS_LIMIT: "100000", ISOLATION: "none"}}, "spec": {
            "automountServiceAccountToken": true,
            "containers": [{"name": "c", "securityContext": {"privileged": true, "allowPrivilegeEscalation": true}}],
        }});
        harden(&mut pod, Some(&n));
        // The server's annotations are not the author's to choose.
        assert_eq!(pod["metadata"]["annotations"][PIDS_LIMIT], "64");
        assert_eq!(pod["metadata"]["annotations"][ISOLATION], "untrusted");
        assert_eq!(pod["spec"]["automountServiceAccountToken"], true);
        let v = violations(enforced(Some(&n)), &pod);
        assert!(v.iter().any(|m| m.starts_with("privileged (")), "{v:?}");
        assert!(v.iter().any(|m| m.starts_with("allowPrivilegeEscalation != false")), "{v:?}");
    }

    #[test]
    fn resources_default_only_where_missing() {
        let mut pod = json!({"spec": {"containers": [
            {"name": "a"},
            {"name": "b", "resources": {"limits": {"memory": "32Mi"}, "requests": {"cpu": "10m"}}},
        ]}});
        default_resources(&mut pod);
        assert_eq!(pod["spec"]["containers"][0]["resources"], json!({"limits": {"cpu": "1", "memory": "512Mi"}, "requests": {"cpu": "50m", "memory": "64Mi"}}));
        // The author's 32Mi limit: the request is not the 64Mi default.
        assert_eq!(pod["spec"]["containers"][1]["resources"], json!({"limits": {"cpu": "1", "memory": "32Mi"}, "requests": {"cpu": "10m", "memory": "32Mi"}}));
    }

    #[test]
    fn updates_keep_the_servers_annotations() {
        let old = json!({"metadata": {"annotations": {ISOLATION: "untrusted", PIDS_LIMIT: "256", "a": "1"}}});
        let mut new = json!({"metadata": {"annotations": {PIDS_LIMIT: "999999", "a": "2"}}});
        keep_annotations(&mut new, &old);
        assert_eq!(new["metadata"]["annotations"], json!({ISOLATION: "untrusted", PIDS_LIMIT: "256", "a": "2"}));
        // A pod of an ordinary namespace cannot claim to be untrusted later,
        // and its own PID limit stays its own.
        let old = json!({"metadata": {"name": "p"}});
        let mut new = json!({"metadata": {"annotations": {ISOLATION: "untrusted", PIDS_LIMIT: "50"}}});
        keep_annotations(&mut new, &old);
        assert_eq!(new["metadata"]["annotations"], json!({PIDS_LIMIT: "50"}));
    }
}
