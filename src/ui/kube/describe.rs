//! `kubectl describe` for the UI: what kubectl prints for pods,
//! deployments, nodes and services, and its generic form (fields as
//! "Title Case" keys) for everything else, custom resources included, with
//! the object's events at the end.

use serde_json::Value;

/// What a describe needs beside the object.
#[derive(Debug, Default, Clone)]
pub struct Related {
    pub events: Vec<Value>,
    /// A deployment's ReplicaSets, a node's pods.
    pub children: Vec<Value>,
    /// A service's Endpoints.
    pub endpoints: Option<Value>,
}

/// Lines with keys aligned like kubectl's tabwriter output.
#[derive(Default)]
struct Out {
    rows: Vec<(usize, Option<String>, String)>,
}

impl Out {
    fn kv(&mut self, indent: usize, k: &str, v: impl Into<String>) {
        let v = v.into();
        self.rows.push((indent, Some(format!("{k}:")), if v.is_empty() { "<none>".into() } else { v }));
    }

    /// A key whose value is the block below it.
    fn head(&mut self, indent: usize, k: &str) {
        self.rows.push((indent, None, format!("{k}:")));
    }

    fn raw(&mut self, indent: usize, s: impl Into<String>) {
        self.rows.push((indent, None, s.into()));
    }

    /// `Labels:` style: one `k=v` per line, aligned under the first.
    fn map(&mut self, indent: usize, k: &str, m: &Value) {
        match m.as_object().filter(|m| !m.is_empty()) {
            None => self.kv(indent, k, "<none>"),
            Some(m) => {
                let mut first = true;
                for (mk, mv) in m {
                    let v = format!("{mk}={}", scalar(mv));
                    if first {
                        self.kv(indent, k, v);
                        first = false;
                    } else {
                        // Continuation lines line up with the value column.
                        self.rows.push((indent, Some(String::new()), v));
                    }
                }
            }
        }
    }

    fn table(&mut self, indent: usize, header: &[&str], rows: Vec<Vec<String>>) {
        let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(c.chars().count());
                }
            }
        }
        let line = |cells: Vec<String>| {
            let n = cells.len();
            cells.into_iter().enumerate().map(|(i, c)| if i + 1 == n { c } else { format!("{c:<w$}", w = widths[i] + 2) }).collect::<String>().trim_end().to_string()
        };
        self.raw(indent, line(header.iter().map(|s| s.to_string()).collect()));
        self.raw(indent, line(header.iter().map(|s| "-".repeat(s.len())).collect()));
        for r in rows {
            self.raw(indent, line(r));
        }
    }

    fn lines(self) -> Vec<String> {
        // Keys at one indent share a width within their block (up to a
        // row indented less), nested blocks and headings in between.
        let n = self.rows.len();
        let mut width = vec![0usize; n];
        let mut i = 0;
        while i < n {
            let (d, key, _) = &self.rows[i];
            if key.is_none() || width[i] > 0 {
                i += 1;
                continue;
            }
            let d = *d;
            let mut start = i;
            while start > 0 && self.rows[start - 1].0 >= d {
                start -= 1;
            }
            let mut end = i;
            while end < n && self.rows[end].0 >= d {
                end += 1;
            }
            let members: Vec<usize> = (start..end).filter(|&j| self.rows[j].0 == d && self.rows[j].1.is_some()).collect();
            let w = members.iter().map(|&j| self.rows[j].1.as_ref().unwrap().chars().count()).max().unwrap_or(0);
            let w = (w + 2).min(40);
            for j in members {
                width[j] = w;
            }
            i += 1;
        }
        self.rows
            .into_iter()
            .enumerate()
            .map(|(j, (ind, k, v))| match k {
                Some(k) => format!("{}{:<w$}{v}", " ".repeat(ind), k, w = width[j]),
                None => format!("{}{v}", " ".repeat(ind)),
            })
            .collect()
    }
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "<none>".into(),
        other => other.to_string(),
    }
}

/// `2026-10-19T10:00:00Z` → `Mon, 19 Oct 2026 10:00:00 +0000`, as kubectl.
fn time(t: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(t).map(|d| d.format("%a, %d %b %Y %H:%M:%S %z").to_string()).unwrap_or_else(|_| t.to_string())
}

/// kubectl's short durations (`5m`, `2d3h`).
pub fn since(now: i64, t: &str) -> String {
    let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) else { return "<unknown>".into() };
    human((now - d.timestamp()).max(0))
}

pub fn human(s: i64) -> String {
    match s {
        0..=119 => format!("{s}s"),
        120..=599 if s % 60 == 0 => format!("{}m", s / 60),
        120..=599 => format!("{}m{}s", s / 60, s % 60),
        600..=10_799 => format!("{}m", s / 60),
        10_800..=86_399 => format!("{}h", s / 3600),
        86_400..=691_199 => {
            let h = (s % 86400) / 3600;
            if h == 0 { format!("{}d", s / 86400) } else { format!("{}d{h}h", s / 86400) }
        }
        _ => format!("{}d", s / 86400),
    }
}

/// `fooBarBaz` → `Foo Bar Baz`; keys with `/`, `.` or `-` stay as they are.
pub fn title(k: &str) -> String {
    if k.contains(['/', '.', '-']) || k.is_empty() {
        return k.to_string();
    }
    let mut out = String::new();
    let chars: Vec<char> = k.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
            continue;
        }
        let prev = chars[i - 1];
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        if c.is_uppercase() && (prev.is_lowercase() || (prev.is_uppercase() && next_lower)) {
            out.push(' ');
        }
        out.push(*c);
    }
    out
}

/// Fields of a generic object, kubectl's way.
fn generic(out: &mut Out, indent: usize, v: &Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                match val {
                    Value::Object(o) if o.is_empty() => out.kv(indent, &title(k), "{}"),
                    Value::Array(a) if a.is_empty() => out.kv(indent, &title(k), "[]"),
                    Value::Object(_) | Value::Array(_) => {
                        out.head(indent, &title(k));
                        generic(out, indent + 2, val);
                    }
                    _ => out.kv(indent, &title(k), scalar(val)),
                }
            }
        }
        Value::Array(a) => {
            for item in a {
                match item {
                    Value::Object(m) if !m.is_empty() => {
                        // kubectl prints list items as blocks, one after another.
                        generic(out, indent, item);
                        out.raw(0, "");
                    }
                    _ => out.raw(indent, scalar(item)),
                }
            }
            if matches!(out.rows.last(), Some((0, None, l)) if l.is_empty()) {
                out.rows.pop();
            }
        }
        other => out.raw(indent, scalar(other)),
    }
}

fn header(out: &mut Out, o: &Value) {
    out.kv(0, "Name", s(o, "/metadata/name"));
    if !s(o, "/metadata/namespace").is_empty() {
        out.kv(0, "Namespace", s(o, "/metadata/namespace"));
    }
    out.map(0, "Labels", &o["metadata"]["labels"]);
    annotations(out, 0, &o["metadata"]["annotations"]);
}

/// Annotations without kubectl's last-applied copy (as describe does).
fn annotations(out: &mut Out, indent: usize, a: &Value) {
    let mut a = a.clone();
    if let Some(m) = a.as_object_mut() {
        m.remove("kubectl.kubernetes.io/last-applied-configuration");
    }
    out.map(indent, "Annotations", &a);
}

/// The object described; `now` (Unix seconds) for ages.
pub fn describe(o: &Value, rel: &Related, now: i64) -> Vec<String> {
    let mut out = Out::default();
    match o["kind"].as_str().unwrap_or_default() {
        "Pod" => pod(&mut out, o, now),
        "Deployment" => deployment(&mut out, o, rel),
        "Node" => node(&mut out, o, rel, now),
        "Service" => service(&mut out, o, rel),
        _ => {
            header(&mut out, o);
            out.kv(0, "API Version", s(o, "/apiVersion"));
            out.kv(0, "Kind", s(o, "/kind"));
            let mut meta = o["metadata"].clone();
            if let Some(m) = meta.as_object_mut() {
                for k in ["name", "namespace", "labels", "annotations", "managedFields"] {
                    m.remove(k);
                }
            }
            out.head(0, "Metadata");
            generic(&mut out, 2, &meta);
            if let Some(m) = o.as_object() {
                for (k, v) in m {
                    if matches!(k.as_str(), "apiVersion" | "kind" | "metadata") {
                        continue;
                    }
                    match v {
                        Value::Object(_) | Value::Array(_) => {
                            out.head(0, &title(k));
                            generic(&mut out, 2, v);
                        }
                        _ => out.kv(0, &title(k), scalar(v)),
                    }
                }
            }
        }
    }
    events(&mut out, &rel.events, now);
    out.lines()
}

fn events(out: &mut Out, evs: &[Value], now: i64) {
    if evs.is_empty() {
        out.kv(0, "Events", "<none>");
        return;
    }
    out.head(0, "Events");
    let mut evs: Vec<&Value> = evs.iter().collect();
    let when = |e: &Value| {
        let t = [s(e, "/lastTimestamp"), s(e, "/eventTime"), s(e, "/metadata/creationTimestamp")].into_iter().find(|t| !t.is_empty()).unwrap_or_default().to_string();
        t
    };
    evs.sort_by_key(|e| when(e));
    let rows = evs
        .iter()
        .map(|e| {
            let count = e["count"].as_i64().unwrap_or(1);
            let first = s(e, "/firstTimestamp");
            let age = if count > 1 && !first.is_empty() { format!("{} (x{count} over {})", since(now, &when(e)), since(now, first)) } else { since(now, &when(e)) };
            let from = [s(e, "/source/component"), s(e, "/reportingComponent")].into_iter().find(|f| !f.is_empty()).unwrap_or_default();
            vec![s(e, "/type").to_string(), s(e, "/reason").to_string(), age, from.to_string(), s(e, "/message").trim().to_string()]
        })
        .collect();
    out.table(2, &["Type", "Reason", "Age", "From", "Message"], rows);
}

fn controlled_by(o: &Value) -> Option<String> {
    o.pointer("/metadata/ownerReferences")?.as_array()?.iter().find(|r| r["controller"].as_bool() == Some(true)).map(|r| format!("{}/{}", s(r, "/kind"), s(r, "/name")))
}

fn state(out: &mut Out, indent: usize, label: &str, st: &Value) {
    let Some((name, d)) = st.as_object().and_then(|m| m.iter().next()) else { return };
    let mut word = title(name);
    if word.is_empty() {
        word = "Unknown".into();
    }
    out.kv(indent, label, word);
    for (k, key) in [("reason", "Reason"), ("message", "Message"), ("exitCode", "Exit Code"), ("signal", "Signal"), ("startedAt", "Started"), ("finishedAt", "Finished")] {
        if let Some(v) = d.get(k) {
            let v = if k.ends_with("At") { time(v.as_str().unwrap_or_default()) } else { scalar(v) };
            out.kv(indent + 2, key, v);
        }
    }
}

fn resources(out: &mut Out, indent: usize, r: &Value) {
    for (k, label) in [("limits", "Limits"), ("requests", "Requests")] {
        if let Some(m) = r[k].as_object().filter(|m| !m.is_empty()) {
            out.head(indent, label);
            for (rk, rv) in m {
                out.kv(indent + 2, rk, scalar(rv));
            }
        }
    }
}

fn probe(p: &Value) -> String {
    let how = if let Some(h) = p.get("httpGet") {
        format!("http-get {}://:{}{}", h["scheme"].as_str().unwrap_or("HTTP").to_lowercase(), scalar(&h["port"]), s(h, "/path"))
    } else if let Some(t) = p.get("tcpSocket") {
        format!("tcp-socket :{}", scalar(&t["port"]))
    } else if let Some(e) = p.get("exec") {
        format!("exec [{}]", e["command"].as_array().map(|a| a.iter().map(scalar).collect::<Vec<_>>().join(" ")).unwrap_or_default())
    } else if p.get("grpc").is_some() {
        format!("grpc :{}", scalar(&p["grpc"]["port"]))
    } else {
        "unknown".into()
    };
    let n = |k: &str, d: i64| p[k].as_i64().unwrap_or(d);
    format!("{how} delay={}s timeout={}s period={}s #success={} #failure={}", n("initialDelaySeconds", 0), n("timeoutSeconds", 1), n("periodSeconds", 10), n("successThreshold", 1), n("failureThreshold", 3))
}

fn containers(out: &mut Out, indent: usize, label: &str, specs: &Value, statuses: &Value) {
    let Some(list) = specs.as_array().filter(|a| !a.is_empty()) else { return };
    out.head(indent, label);
    for c in list {
        let name = s(c, "/name");
        let st = statuses.as_array().and_then(|a| a.iter().find(|x| s(x, "/name") == name)).cloned().unwrap_or(Value::Null);
        let i = indent + 2;
        out.head(i, name);
        if !st.is_null() {
            out.kv(i + 2, "Container ID", s(&st, "/containerID"));
        }
        out.kv(i + 2, "Image", s(c, "/image"));
        if !st.is_null() {
            out.kv(i + 2, "Image ID", s(&st, "/imageID"));
        }
        let ports: Vec<String> = c["ports"].as_array().into_iter().flatten().map(|p| format!("{}/{}", scalar(&p["containerPort"]), p["protocol"].as_str().unwrap_or("TCP"))).collect();
        out.kv(i + 2, if ports.len() > 1 { "Ports" } else { "Port" }, ports.join(", "));
        if let Some(cmd) = c["command"].as_array() {
            out.head(i + 2, "Command");
            for x in cmd {
                out.raw(i + 4, scalar(x));
            }
        }
        if let Some(args) = c["args"].as_array() {
            out.head(i + 2, "Args");
            for x in args {
                out.raw(i + 4, scalar(x));
            }
        }
        if !st.is_null() {
            state(out, i + 2, "State", &st["state"]);
            if st["lastState"].as_object().is_some_and(|m| !m.is_empty()) {
                state(out, i + 2, "Last State", &st["lastState"]);
            }
            out.kv(i + 2, "Ready", if st["ready"].as_bool() == Some(true) { "True" } else { "False" });
            out.kv(i + 2, "Restart Count", scalar(&st["restartCount"]));
        }
        resources(out, i + 2, &c["resources"]);
        for (k, label) in [("livenessProbe", "Liveness"), ("readinessProbe", "Readiness"), ("startupProbe", "Startup")] {
            if c.get(k).is_some() {
                out.kv(i + 2, label, probe(&c[k]));
            }
        }
        let env = c["env"].as_array().cloned().unwrap_or_default();
        if env.is_empty() && c["envFrom"].is_null() {
            out.kv(i + 2, "Environment", "<none>");
        } else {
            out.head(i + 2, "Environment");
            for e in env {
                let v = if let Some(v) = e.get("value") {
                    scalar(v)
                } else if let Some(r) = e.pointer("/valueFrom/secretKeyRef") {
                    format!("<set to the key '{}' in secret '{}'>", s(r, "/key"), s(r, "/name"))
                } else if let Some(r) = e.pointer("/valueFrom/configMapKeyRef") {
                    format!("<set to the key '{}' of config map '{}'>", s(r, "/key"), s(r, "/name"))
                } else if let Some(r) = e.pointer("/valueFrom/fieldRef") {
                    format!("({}:{})", r["apiVersion"].as_str().unwrap_or("v1"), s(r, "/fieldPath"))
                } else if let Some(r) = e.pointer("/valueFrom/resourceFieldRef") {
                    format!("{} ({})", s(r, "/resource"), s(r, "/containerName"))
                } else {
                    String::new()
                };
                out.kv(i + 4, s(&e, "/name"), v);
            }
            for f in c["envFrom"].as_array().into_iter().flatten() {
                if let Some(r) = f.get("configMapRef") {
                    out.raw(i + 4, format!("{}  ConfigMap  Optional: {}", s(r, "/name"), r["optional"].as_bool().unwrap_or(false)));
                }
                if let Some(r) = f.get("secretRef") {
                    out.raw(i + 4, format!("{}  Secret  Optional: {}", s(r, "/name"), r["optional"].as_bool().unwrap_or(false)));
                }
            }
        }
        let mounts: Vec<String> = c["volumeMounts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| format!("{} from {} ({})", s(m, "/mountPath"), s(m, "/name"), if m["readOnly"].as_bool() == Some(true) { "ro" } else { "rw" }))
            .collect();
        if mounts.is_empty() {
            out.kv(i + 2, "Mounts", "<none>");
        } else {
            out.head(i + 2, "Mounts");
            for m in mounts {
                out.raw(i + 4, m);
            }
        }
    }
}

fn volumes(out: &mut Out, vols: &Value) {
    let Some(list) = vols.as_array().filter(|a| !a.is_empty()) else {
        out.kv(0, "Volumes", "<none>");
        return;
    };
    out.head(0, "Volumes");
    for v in list {
        out.head(2, s(v, "/name"));
        let Some((kind, d)) = v.as_object().and_then(|m| m.iter().find(|(k, _)| *k != "name")) else { continue };
        let what = match kind.as_str() {
            "persistentVolumeClaim" => "PersistentVolumeClaim (a reference to a PersistentVolumeClaim in the same namespace)",
            "configMap" => "ConfigMap (a volume populated by a ConfigMap)",
            "secret" => "Secret (a volume populated by a Secret)",
            "emptyDir" => "EmptyDir (a temporary directory that shares a pod's lifetime)",
            "hostPath" => "HostPath (bare host directory volume)",
            "projected" => "Projected (a volume that contains injected data from multiple sources)",
            "downwardAPI" => "DownwardAPI (a volume populated by information about the pod)",
            _ => kind.as_str(),
        };
        out.kv(4, "Type", what);
        if let Some(m) = d.as_object() {
            for (k, val) in m {
                if !val.is_object() && !val.is_array() {
                    out.kv(4, &title(k), scalar(val));
                }
            }
        }
    }
}

fn pod(out: &mut Out, o: &Value, now: i64) {
    let _ = now;
    out.kv(0, "Name", s(o, "/metadata/name"));
    out.kv(0, "Namespace", s(o, "/metadata/namespace"));
    out.kv(0, "Priority", o.pointer("/spec/priority").map(scalar).unwrap_or_else(|| "0".into()));
    out.kv(0, "Service Account", s(o, "/spec/serviceAccountName"));
    let node = s(o, "/spec/nodeName");
    out.kv(0, "Node", if node.is_empty() { "<none>".into() } else { format!("{node}/{}", s(o, "/status/hostIP")) });
    if !s(o, "/status/startTime").is_empty() {
        out.kv(0, "Start Time", time(s(o, "/status/startTime")));
    }
    out.map(0, "Labels", &o["metadata"]["labels"]);
    annotations(out, 0, &o["metadata"]["annotations"]);
    let deleting = !s(o, "/metadata/deletionTimestamp").is_empty();
    out.kv(0, "Status", if deleting { format!("Terminating (lasts {}s)", o.pointer("/metadata/deletionGracePeriodSeconds").map(scalar).unwrap_or_default()) } else { s(o, "/status/phase").to_string() });
    if !s(o, "/status/reason").is_empty() {
        out.kv(0, "Reason", s(o, "/status/reason"));
    }
    if !s(o, "/status/message").is_empty() {
        out.kv(0, "Message", s(o, "/status/message"));
    }
    out.kv(0, "IP", s(o, "/status/podIP"));
    if let Some(ips) = o.pointer("/status/podIPs").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        out.head(0, "IPs");
        for ip in ips {
            out.kv(2, "IP", s(ip, "/ip"));
        }
    }
    if let Some(c) = controlled_by(o) {
        out.kv(0, "Controlled By", c);
    }
    containers(out, 0, "Init Containers", &o["spec"]["initContainers"], &o["status"]["initContainerStatuses"]);
    containers(out, 0, "Containers", &o["spec"]["containers"], &o["status"]["containerStatuses"]);
    if let Some(conds) = o.pointer("/status/conditions").and_then(Value::as_array) {
        out.head(0, "Conditions");
        out.table(2, &["Type", "Status"], conds.iter().map(|c| vec![s(c, "/type").to_string(), s(c, "/status").to_string()]).collect());
    }
    volumes(out, &o["spec"]["volumes"]);
    out.kv(0, "QoS Class", s(o, "/status/qosClass"));
    out.map(0, "Node-Selectors", &o["spec"]["nodeSelector"]);
    let tol: Vec<String> = o["spec"]["tolerations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let mut x = s(t, "/key").to_string();
            if !s(t, "/value").is_empty() {
                x.push_str(&format!("={}", s(t, "/value")));
            }
            if !s(t, "/effect").is_empty() {
                x.push_str(&format!(":{}", s(t, "/effect")));
            }
            if s(t, "/operator") == "Exists" && s(t, "/value").is_empty() {
                x.push_str(" op=Exists");
            }
            if let Some(sec) = t["tolerationSeconds"].as_i64() {
                x.push_str(&format!(" for {sec}s"));
            }
            x
        })
        .collect();
    if tol.is_empty() {
        out.kv(0, "Tolerations", "<none>");
    } else {
        out.kv(0, "Tolerations", tol[0].clone());
        for t in &tol[1..] {
            out.rows.push((0, Some(String::new()), t.clone()));
        }
    }
}

fn selector_text(sel: &Value) -> String {
    let mut parts: Vec<String> = sel["matchLabels"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", scalar(v))).collect();
    for e in sel["matchExpressions"].as_array().into_iter().flatten() {
        let vals: Vec<String> = e["values"].as_array().into_iter().flatten().map(scalar).collect();
        parts.push(match s(e, "/operator") {
            "In" => format!("{} in ({})", s(e, "/key"), vals.join(",")),
            "NotIn" => format!("{} notin ({})", s(e, "/key"), vals.join(",")),
            "Exists" => s(e, "/key").to_string(),
            _ => format!("!{}", s(e, "/key")),
        });
    }
    parts.join(",")
}

fn deployment(out: &mut Out, o: &Value, rel: &Related) {
    out.kv(0, "Name", s(o, "/metadata/name"));
    out.kv(0, "Namespace", s(o, "/metadata/namespace"));
    out.kv(0, "CreationTimestamp", time(s(o, "/metadata/creationTimestamp")));
    out.map(0, "Labels", &o["metadata"]["labels"]);
    annotations(out, 0, &o["metadata"]["annotations"]);
    out.kv(0, "Selector", selector_text(&o["spec"]["selector"]));
    let n = |p: &str| o.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    out.kv(
        0,
        "Replicas",
        format!(
            "{} desired | {} updated | {} total | {} available | {} unavailable",
            o.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1),
            n("/status/updatedReplicas"),
            n("/status/replicas"),
            n("/status/availableReplicas"),
            n("/status/unavailableReplicas")
        ),
    );
    let strategy = s(o, "/spec/strategy/type");
    out.kv(0, "StrategyType", strategy);
    out.kv(0, "MinReadySeconds", n("/spec/minReadySeconds").to_string());
    if strategy == "RollingUpdate" {
        let ru = &o["spec"]["strategy"]["rollingUpdate"];
        out.kv(0, "RollingUpdateStrategy", format!("{} max unavailable, {} max surge", ru.get("maxUnavailable").map(scalar).unwrap_or("25%".into()), ru.get("maxSurge").map(scalar).unwrap_or("25%".into())));
    }
    out.head(0, "Pod Template");
    out.map(2, "Labels", &o["spec"]["template"]["metadata"]["labels"]);
    if o.pointer("/spec/template/metadata/annotations").is_some() {
        annotations(out, 2, &o["spec"]["template"]["metadata"]["annotations"]);
    }
    let tpl = &o["spec"]["template"]["spec"];
    if !s(tpl, "/serviceAccountName").is_empty() {
        out.kv(2, "Service Account", s(tpl, "/serviceAccountName"));
    }
    containers(out, 2, "Init Containers", &tpl["initContainers"], &Value::Null);
    containers(out, 2, "Containers", &tpl["containers"], &Value::Null);
    if let Some(conds) = o.pointer("/status/conditions").and_then(Value::as_array) {
        out.head(0, "Conditions");
        out.table(2, &["Type", "Status", "Reason"], conds.iter().map(|c| vec![s(c, "/type").to_string(), s(c, "/status").to_string(), s(c, "/reason").to_string()]).collect());
    }
    // ReplicaSets: the one with the current template's hash is the new one.
    let uid = s(o, "/metadata/uid");
    let mine: Vec<&Value> = rel.children.iter().filter(|r| r["metadata"]["ownerReferences"].as_array().is_some_and(|a| a.iter().any(|x| s(x, "/uid") == uid))).collect();
    let rev = |r: &Value| s(r, "/metadata/annotations/deployment.kubernetes.io~1revision").parse::<i64>().unwrap_or(0);
    let newest = mine.iter().max_by_key(|r| rev(r)).map(|r| s(r, "/metadata/name").to_string());
    let line = |r: &Value| format!("{} ({}/{} replicas created)", s(r, "/metadata/name"), r.pointer("/status/replicas").map(scalar).unwrap_or("0".into()), r.pointer("/spec/replicas").map(scalar).unwrap_or("0".into()));
    let old: Vec<String> = mine.iter().filter(|r| Some(s(r, "/metadata/name").to_string()) != newest && r.pointer("/status/replicas").and_then(Value::as_i64).unwrap_or(0) > 0).map(|r| line(r)).collect();
    out.kv(0, "OldReplicaSets", if old.is_empty() { "<none>".into() } else { old.join(", ") });
    out.kv(0, "NewReplicaSet", mine.iter().find(|r| Some(s(r, "/metadata/name").to_string()) == newest).map(|r| line(r)).unwrap_or_else(|| "<none>".into()));
}

fn node(out: &mut Out, o: &Value, rel: &Related, now: i64) {
    out.kv(0, "Name", s(o, "/metadata/name"));
    let roles: Vec<String> = o["metadata"]["labels"].as_object().into_iter().flatten().filter_map(|(k, _)| k.strip_prefix("node-role.kubernetes.io/").map(str::to_string)).collect();
    out.kv(0, "Roles", if roles.is_empty() { "<none>".into() } else { roles.join(",") });
    out.map(0, "Labels", &o["metadata"]["labels"]);
    annotations(out, 0, &o["metadata"]["annotations"]);
    out.kv(0, "CreationTimestamp", time(s(o, "/metadata/creationTimestamp")));
    let taints: Vec<String> = o["spec"]["taints"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| if s(t, "/value").is_empty() { format!("{}:{}", s(t, "/key"), s(t, "/effect")) } else { format!("{}={}:{}", s(t, "/key"), s(t, "/value"), s(t, "/effect")) })
        .collect();
    out.kv(0, "Taints", if taints.is_empty() { "<none>".into() } else { taints.join(", ") });
    out.kv(0, "Unschedulable", if o.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true) { "true" } else { "false" });
    if let Some(conds) = o.pointer("/status/conditions").and_then(Value::as_array) {
        out.head(0, "Conditions");
        out.table(
            2,
            &["Type", "Status", "LastHeartbeatTime", "Reason", "Message"],
            conds.iter().map(|c| vec![s(c, "/type").to_string(), s(c, "/status").to_string(), since(now, s(c, "/lastHeartbeatTime")) + " ago", s(c, "/reason").to_string(), s(c, "/message").to_string()]).collect(),
        );
    }
    out.head(0, "Addresses");
    for a in o["status"]["addresses"].as_array().into_iter().flatten() {
        out.kv(2, s(a, "/type"), s(a, "/address"));
    }
    for (k, label) in [("capacity", "Capacity"), ("allocatable", "Allocatable")] {
        if let Some(m) = o["status"][k].as_object() {
            out.head(0, label);
            for (rk, rv) in m {
                out.kv(2, rk, scalar(rv));
            }
        }
    }
    if let Some(info) = o.pointer("/status/nodeInfo").and_then(Value::as_object) {
        out.head(0, "System Info");
        for (k, label) in [
            ("machineID", "Machine ID"),
            ("systemUUID", "System UUID"),
            ("bootID", "Boot ID"),
            ("kernelVersion", "Kernel Version"),
            ("osImage", "OS Image"),
            ("operatingSystem", "Operating System"),
            ("architecture", "Architecture"),
            ("containerRuntimeVersion", "Container Runtime Version"),
            ("kubeletVersion", "Kubelet Version"),
            ("kubeProxyVersion", "Kube-Proxy Version"),
        ] {
            if let Some(v) = info.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                out.kv(2, label, v);
            }
        }
    }
    if !s(o, "/spec/podCIDR").is_empty() {
        out.kv(0, "PodCIDR", s(o, "/spec/podCIDR"));
    }
    if !rel.children.is_empty() {
        let pods: Vec<&Value> = rel.children.iter().filter(|p| !matches!(s(p, "/status/phase"), "Succeeded" | "Failed")).collect();
        out.head(0, &format!("Non-terminated Pods ({} in total)", pods.len()));
        let sum = |p: &Value, kind: &str, res: &str| -> f64 {
            p["spec"]["containers"].as_array().into_iter().flatten().filter_map(|c| c["resources"][kind][res].as_str().and_then(super::table::quantity)).sum::<f64>() + 0.0
        };
        let q = |v: f64, res: &str| if v == 0.0 { "0".to_string() } else if res == "cpu" { format!("{}m", (v * 1000.0).round()) } else { format!("{}Mi", (v / 1048576.0).round()) };
        let rows = pods
            .iter()
            .map(|p| {
                vec![
                    s(p, "/metadata/namespace").to_string(),
                    s(p, "/metadata/name").to_string(),
                    q(sum(p, "requests", "cpu"), "cpu"),
                    q(sum(p, "limits", "cpu"), "cpu"),
                    q(sum(p, "requests", "memory"), "memory"),
                    q(sum(p, "limits", "memory"), "memory"),
                    since(now, s(p, "/metadata/creationTimestamp")),
                ]
            })
            .collect();
        out.table(2, &["Namespace", "Name", "CPU Requests", "CPU Limits", "Memory Requests", "Memory Limits", "Age"], rows);
        let alloc = |res: &str| o["status"]["allocatable"][res].as_str().and_then(super::table::quantity).unwrap_or(0.0);
        let pct = |v: f64, res: &str| if alloc(res) > 0.0 { format!("{} ({}%)", q(v, res), (v / alloc(res) * 100.0).round()) } else { q(v, res) };
        let total = |kind: &str, res: &str| pods.iter().map(|p| sum(p, kind, res)).sum::<f64>() + 0.0;
        out.head(0, "Allocated resources");
        out.table(
            2,
            &["Resource", "Requests", "Limits"],
            vec![
                vec!["cpu".into(), pct(total("requests", "cpu"), "cpu"), pct(total("limits", "cpu"), "cpu")],
                vec!["memory".into(), pct(total("requests", "memory"), "memory"), pct(total("limits", "memory"), "memory")],
            ],
        );
    }
}

fn service(out: &mut Out, o: &Value, rel: &Related) {
    header(out, o);
    let sel = o["spec"]["selector"].as_object().map(|m| m.iter().map(|(k, v)| format!("{k}={}", scalar(v))).collect::<Vec<_>>().join(",")).unwrap_or_default();
    out.kv(0, "Selector", sel);
    out.kv(0, "Type", s(o, "/spec/type"));
    if !s(o, "/spec/ipFamilyPolicy").is_empty() {
        out.kv(0, "IP Family Policy", s(o, "/spec/ipFamilyPolicy"));
    }
    out.kv(0, "IP", s(o, "/spec/clusterIP"));
    if let Some(ips) = o.pointer("/spec/clusterIPs").and_then(Value::as_array) {
        out.kv(0, "IPs", ips.iter().map(scalar).collect::<Vec<_>>().join(","));
    }
    if let Some(lb) = o.pointer("/status/loadBalancer/ingress").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        out.kv(0, "LoadBalancer Ingress", lb.iter().map(|i| [s(i, "/ip"), s(i, "/hostname")].concat()).collect::<Vec<_>>().join(", "));
    }
    let eps: Vec<String> = rel
        .endpoints
        .as_ref()
        .and_then(|e| e["subsets"].as_array())
        .into_iter()
        .flatten()
        .flat_map(|ss| {
            let ports: Vec<String> = ss["ports"].as_array().into_iter().flatten().map(|p| scalar(&p["port"])).collect();
            ss["addresses"].as_array().cloned().unwrap_or_default().into_iter().flat_map(move |a| {
                let ip = s(&a, "/ip").to_string();
                ports.clone().into_iter().map(move |p| format!("{ip}:{p}"))
            })
        })
        .collect();
    for p in o["spec"]["ports"].as_array().into_iter().flatten() {
        let name = p["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("<unset>");
        out.kv(0, "Port", format!("{name}  {}/{}", scalar(&p["port"]), p["protocol"].as_str().unwrap_or("TCP")));
        out.kv(0, "TargetPort", format!("{}/{}", p.get("targetPort").map(scalar).unwrap_or_else(|| scalar(&p["port"])), p["protocol"].as_str().unwrap_or("TCP")));
        if let Some(np) = p.get("nodePort") {
            out.kv(0, "NodePort", format!("{name}  {}/{}", scalar(np), p["protocol"].as_str().unwrap_or("TCP")));
        }
    }
    out.kv(0, "Endpoints", if eps.is_empty() { "<none>".into() } else if eps.len() > 3 { format!("{} + {} more...", eps[..3].join(","), eps.len() - 3) } else { eps.join(",") });
    out.kv(0, "Session Affinity", s(o, "/spec/sessionAffinity"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_like_kubectl() {
        assert_eq!(title("creationTimestamp"), "Creation Timestamp");
        assert_eq!(title("podIP"), "Pod IP");
        assert_eq!(title("apiVersion"), "Api Version");
        assert_eq!(title("app.kubernetes.io/name"), "app.kubernetes.io/name");
        assert_eq!(human(45), "45s");
        assert_eq!(human(150), "2m30s");
        assert_eq!(human(3 * 3600), "3h");
        assert_eq!(human(86400 + 3 * 3600), "1d3h");
    }

    #[test]
    fn describes_a_pod_with_events() {
        let pod = json!({"kind": "Pod", "apiVersion": "v1",
            "metadata": {"name": "web-1", "namespace": "default", "labels": {"app": "web", "tier": "front"}, "ownerReferences": [{"kind": "ReplicaSet", "name": "web-7d9", "controller": true}]},
            "spec": {"nodeName": "n1", "containers": [{"name": "nginx", "image": "nginx:1.27", "ports": [{"containerPort": 80}], "env": [{"name": "A", "value": "1"}, {"name": "S", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}], "resources": {"limits": {"memory": "128Mi"}}}]},
            "status": {"phase": "Running", "podIP": "10.0.0.5", "hostIP": "192.168.1.2", "qosClass": "Burstable",
                "containerStatuses": [{"name": "nginx", "ready": true, "restartCount": 2, "state": {"running": {"startedAt": "2026-10-19T10:00:00Z"}}, "lastState": {"terminated": {"reason": "Error", "exitCode": 1}}}],
                "conditions": [{"type": "Ready", "status": "True"}]}});
        let ev = json!({"type": "Warning", "reason": "BackOff", "message": "Back-off restarting", "count": 3, "firstTimestamp": "2026-10-19T10:00:00Z", "lastTimestamp": "2026-10-19T10:05:00Z", "source": {"component": "kubelet"}});
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-19T10:10:00Z").unwrap().timestamp();
        let lines = describe(&pod, &Related { events: vec![ev], ..Default::default() }, now);
        let text = lines.join("\n");
        for want in [
            "Name:             web-1",
            "Node:             n1/192.168.1.2",
            "Labels:           app=web",
            "                  tier=front",
            "Controlled By:    ReplicaSet/web-7d9",
            "    Port:           80/TCP",
            "    State:          Running",
            "    Last State:     Terminated",
            "      Reason:     Error",
            "      Exit Code:  1",
            "QoS Class:        Burstable",
            "    Restart Count:  2",
            "      S:  <set to the key 'k' in secret 's'>",
            "  Type     Reason   Age               From     Message",
            "  Warning  BackOff  5m (x3 over 10m)  kubelet  Back-off restarting",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn describes_anything_generically() {
        let o = json!({"apiVersion": "cert-manager.io/v1", "kind": "Certificate", "metadata": {"name": "c", "namespace": "n", "uid": "u"},
            "spec": {"dnsNames": ["a.example", "b.example"], "issuerRef": {"name": "le", "kind": "ClusterIssuer"}}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}});
        let text = describe(&o, &Related::default(), 0).join("\n");
        for want in ["Kind:         Certificate", "Spec:", "  Dns Names:", "    a.example", "  Issuer Ref:", "    Kind:  ClusterIssuer", "Status:", "    Type:    Ready", "Events:       <none>"] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }
}
