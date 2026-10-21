//! `litek8s logs` (search across pods and nodes), `litek8s alerts` and
//! `litek8s query`, against a litek8s API server from the kubeconfig.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde_json::{Value, json};

use crate::ui::client::{Endpoint, Lines, enc};

/// The API server: `--context`, else `$KUBECONFIG`'s current context, else
/// the local dev cluster or the kubeconfig's (as `litek8s ui`).
pub fn endpoint(context: Option<&str>) -> Result<Endpoint> {
    if context.is_none() && std::env::var_os("KUBECONFIG").is_some() {
        let path = crate::dev::kubeconfig::default_path();
        let doc: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?)?;
        if let Some(cur) = doc["current-context"].as_str().filter(|c| !c.is_empty()) {
            return Ok(crate::ui::resolve_kube(Some(cur))?.0);
        }
    }
    Ok(crate::ui::resolve_kube(context)?.0)
}

#[derive(Debug, clap::Args)]
pub struct LogsArgs {
    /// A regex on pod names.
    pub name: Option<String>,
    #[arg(short = 'n', long, help = "Only this namespace (default: all)")]
    pub namespace: Option<String>,
    #[arg(short = 'A', long, help = "All namespaces (the default)")]
    pub all_namespaces: bool,
    #[arg(short = 'l', long, help = "Pod label selector, e.g. app=web,tier!=db")]
    pub selector: Option<String>,
    #[arg(short = 'c', long, help = "Only this container")]
    pub container: Option<String>,
    #[arg(long, help = "Lines since a duration ago (1h, 30m) or an RFC 3339 time")]
    pub since: Option<String>,
    #[arg(long, help = "Lines until a duration ago or an RFC 3339 time")]
    pub until: Option<String>,
    #[arg(short = 'g', long, help = "Only lines matching this regex ((?i) for any case)")]
    pub grep: Option<String>,
    #[arg(short = 'f', long, help = "Keep printing new lines (and pods that start later)")]
    pub follow: bool,
    #[arg(long, help = "The newest this many lines (default 1000)")]
    pub limit: Option<usize>,
    #[arg(long, help = "Only this node")]
    pub node: Option<String>,
    #[arg(long, help = "Leave out logs of containers that are gone")]
    pub live_only: bool,
    #[arg(long, help = "Print JSON lines (same as -o json)")]
    pub json: bool,
    #[arg(short = 'o', long, value_parser = ["text", "json"], help = "Output format: json prints one JSON object per line")]
    pub output: Option<String>,
    #[arg(long, help = "kubeconfig context")]
    pub context: Option<String>,
}

const COLORS: [&str; 6] = ["36", "33", "35", "32", "34", "31"];

fn print_line(v: &Value, json_out: bool, colors: &mut BTreeMap<String, &'static str>, tty: bool) {
    let mut out = std::io::stdout().lock();
    if json_out {
        let _ = writeln!(out, "{v}");
        return;
    }
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        eprintln!("litek8s: node {}: {e}", v.get("node").and_then(Value::as_str).unwrap_or("?"));
        return;
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    let ts = chrono::DateTime::parse_from_rfc3339(s("ts")).map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S%.3f").to_string()).unwrap_or_default();
    let who = format!("{}/{} {}", s("namespace"), s("pod"), s("container"));
    if tty {
        let n = colors.len();
        let c = *colors.entry(who.clone()).or_insert(COLORS[n % COLORS.len()]);
        let err = if s("stream") == "stderr" { "\x1b[2m!\x1b[0m" } else { " " };
        let _ = writeln!(out, "\x1b[2m{ts}\x1b[0m \x1b[{c}m{who}\x1b[0m{err}{}", s("line"));
    } else {
        let _ = writeln!(out, "{ts} {who} {}", s("line"));
    }
}

pub async fn logs(a: LogsArgs) -> Result<()> {
    let ep = endpoint(a.context.as_deref())?;
    let mut q = Vec::new();
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            q.push(format!("{k}={}", enc(&v)));
        }
    };
    add("namespace", a.namespace.filter(|_| !a.all_namespaces));
    add("selector", a.selector);
    add("pod", a.name);
    add("container", a.container);
    add("since", a.since);
    add("until", a.until);
    add("grep", a.grep);
    add("limit", a.limit.map(|l| l.to_string()));
    add("node", a.node);
    if a.follow {
        q.push("follow=true".into());
    }
    if a.live_only {
        q.push("liveOnly=true".into());
    }
    let path = format!("/litek8s/v1/logs?{}", q.join("&"));
    let resp: hyper::Response<hyper::body::Incoming> = ep.send(Method::GET, &path, None, Vec::new()).await.with_context(|| format!("cannot reach {}", ep.display))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = http_body_util::BodyExt::collect(resp.into_body()).await.map(|b| b.to_bytes()).unwrap_or_default();
        let msg = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["error"].as_str().or(v["message"].as_str()).map(str::to_string));
        bail!("{}", msg.unwrap_or_else(|| format!("{status}: {}", String::from_utf8_lossy(&body))));
    }
    let tty = std::io::stdout().is_terminal();
    let mut colors = BTreeMap::new();
    let mut lines = Lines::new(resp.into_body());
    let json_out = a.json || a.output.as_deref() == Some("json");
    while let Some(line) = lines.next().await {
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            print_line(&v, json_out, &mut colors, tty);
        }
    }
    Ok(())
}

#[derive(Debug, clap::Subcommand)]
pub enum AlertsCommand {
    /// Active alerts (the default)
    List {
        #[arg(long, help = "Also resolved ones (kept 15 minutes)")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mute alerts matching labels (alertname=X namespace=~prod.*) for a while
    Silence {
        /// name=value, name!=value, name=~regex, name!~regex
        #[arg(required = true)]
        matchers: Vec<String>,
        #[arg(long = "for", default_value = "2h", help = "How long (e.g. 30m, 2h, 7d)")]
        duration: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Acknowledge alerts (by fingerprint or alert name): muted until they resolve
    Ack {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Remove a silence
    Unsilence { name: String },
    /// Silences and their state
    Silences,
}

#[derive(Debug, clap::Args)]
pub struct AlertsArgs {
    #[command(subcommand)]
    pub command: Option<AlertsCommand>,
    #[arg(long, global = true, help = "kubeconfig context")]
    pub context: Option<String>,
    #[arg(short = 'o', long, global = true, value_parser = ["text", "json"], help = "Output format")]
    pub output: Option<String>,
}

fn target(labels: &Value) -> String {
    ["namespace", "pod", "container", "node", "persistentvolumeclaim"].iter().filter_map(|k| labels.get(*k).and_then(Value::as_str)).collect::<Vec<_>>().join("/")
}

fn ago(ms: i64) -> String {
    let secs = (crate::obs::tsdb::now_ms() - ms).max(0) / 1000;
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

pub(crate) fn silence_object(matchers: Vec<Value>, ends_ms: i64, comment: &str, until_resolved: bool) -> Value {
    let mut meta = json!({"generateName": "silence-"});
    if until_resolved {
        meta["annotations"] = json!({crate::obs::alerts::UNTIL_RESOLVED: "true"});
    }
    let user = std::env::var("USER").unwrap_or_default();
    json!({
        "apiVersion": format!("{}/v1", crate::obs::alerts::GROUP),
        "kind": "Silence",
        "metadata": meta,
        "spec": {
            "matchers": matchers,
            "startsAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "endsAt": chrono::DateTime::from_timestamp_millis(ends_ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "comment": comment,
            "createdBy": user,
        },
    })
}

pub(crate) fn parse_matcher(m: &str) -> Result<Value> {
    for (op, re, eq) in [("=~", true, true), ("!~", true, false), ("!=", false, false), ("=", false, true)] {
        if let Some((name, value)) = m.split_once(op) {
            return Ok(json!({"name": name.trim(), "value": value.trim().trim_matches('"'), "isRegex": re, "isEqual": eq}));
        }
    }
    bail!("bad matcher {m:?}: use name=value, name!=value, name=~regex or name!~regex")
}

pub(crate) const SILENCES: &str = "/apis/observability.litek8s.dev/v1/silences";

pub async fn alerts(a: AlertsArgs) -> Result<()> {
    let ep = endpoint(a.context.as_deref())?;
    let json_out = a.output.as_deref() == Some("json");
    match a.command.unwrap_or(AlertsCommand::List { all: false, json: false }) {
        AlertsCommand::List { all, json } => {
            let mut v = ep.json(Method::GET, "/litek8s/v1/alerts", None).await?;
            if json || json_out {
                if !all {
                    if let Some(rows) = v["alerts"].as_array_mut() {
                        rows.retain(|r| r["state"] != "resolved");
                    }
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let mut rows: Vec<&Value> = v["alerts"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
            rows.retain(|r| all || r["state"] != "resolved");
            rows.sort_by_key(|r| (r["state"] != "firing", r["labels"]["severity"] != "critical", r["active_at"].as_i64()));
            if rows.is_empty() {
                println!("No alerts.");
            } else {
                println!("{:<9} {:<28} {:<40} {:<8} {:<6} {:<16} SUMMARY", "STATE", "ALERT", "TARGET", "SEVERITY", "SINCE", "FINGERPRINT");
                for r in rows {
                    let mut state = r["state"].as_str().unwrap_or_default().to_string();
                    if r["silenced_by"].as_array().is_some_and(|s| !s.is_empty()) {
                        state = format!("{state}*");
                    }
                    println!(
                        "{:<9} {:<28} {:<40} {:<8} {:<6} {:<16} {}",
                        state,
                        r["labels"]["alertname"].as_str().unwrap_or_default(),
                        target(&r["labels"]),
                        r["labels"]["severity"].as_str().unwrap_or_default(),
                        ago(r["active_at"].as_i64().unwrap_or_default()),
                        r["fingerprint"].as_str().unwrap_or_default(),
                        r["annotations"]["summary"].as_str().unwrap_or_default()
                    );
                }
                println!("(* silenced)");
            }
            for (rule, e) in v["ruleErrors"].as_object().into_iter().flatten() {
                eprintln!("rule {rule}: {}", e.as_str().unwrap_or_default());
            }
            for (r, e) in v["receiverErrors"].as_object().into_iter().flatten() {
                eprintln!("receiver {r}: {}", e.as_str().unwrap_or_default());
            }
        }
        AlertsCommand::Silence { matchers, duration, comment } => {
            let ms = matchers.iter().map(|m| parse_matcher(m)).collect::<Result<Vec<_>>>()?;
            let d = crate::obs::tsdb::parse_duration(&duration)?;
            let obj = silence_object(ms, crate::obs::tsdb::now_ms() + d.as_millis() as i64, &comment, false);
            let created = ep.json(Method::POST, SILENCES, Some(&obj)).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&created)?);
                return Ok(());
            }
            println!("silence {} created (until {})", created["metadata"]["name"].as_str().unwrap_or_default(), created["spec"]["endsAt"].as_str().unwrap_or_default());
        }
        AlertsCommand::Ack { id, comment } => {
            let v = ep.json(Method::GET, "/litek8s/v1/alerts", None).await?;
            let targets: Vec<&Value> =
                v["alerts"].as_array().into_iter().flatten().filter(|a| a["state"] != "resolved" && (a["fingerprint"] == id.as_str() || a["labels"]["alertname"] == id.as_str())).collect();
            if targets.is_empty() {
                bail!("no active alert with fingerprint or name {id:?} (see `litek8s alerts`)");
            }
            let mut out = Vec::new();
            for t in targets {
                let ms: Vec<Value> = t["labels"].as_object().into_iter().flatten().map(|(k, v)| json!({"name": k, "value": v, "isRegex": false, "isEqual": true})).collect();
                // Until it resolves (the silence goes away then), at most a week.
                let obj = silence_object(ms, crate::obs::tsdb::now_ms() + 7 * 86_400_000, &comment, true);
                let created = ep.json(Method::POST, SILENCES, Some(&obj)).await?;
                if json_out {
                    out.push(created);
                    continue;
                }
                println!("acknowledged {} {} (silence {})", t["labels"]["alertname"].as_str().unwrap_or_default(), target(&t["labels"]), created["metadata"]["name"].as_str().unwrap_or_default());
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
        }
        AlertsCommand::Unsilence { name } => {
            let deleted = ep.json(Method::DELETE, &format!("{SILENCES}/{}", enc(&name)), None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&deleted)?);
                return Ok(());
            }
            println!("silence {name} deleted");
        }
        AlertsCommand::Silences => {
            let v = ep.json(Method::GET, SILENCES, None).await?;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            if json_out {
                println!("{}", serde_json::to_string_pretty(&items)?);
                return Ok(());
            }
            if items.is_empty() {
                println!("No silences.");
            }
            for s in items {
                let ms: Vec<String> = s["spec"]["matchers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        let op = match (m["isRegex"].as_bool().unwrap_or(false), m["isEqual"].as_bool().unwrap_or(true)) {
                            (false, true) => "=",
                            (false, false) => "!=",
                            (true, true) => "=~",
                            (true, false) => "!~",
                        };
                        format!("{}{op}{}", m["name"].as_str().unwrap_or_default(), m["value"].as_str().unwrap_or_default())
                    })
                    .collect();
                println!("{:<20} until {:<22} {} {}", s["metadata"]["name"].as_str().unwrap_or_default(), s["spec"]["endsAt"].as_str().unwrap_or_default(), ms.join(","), s["spec"]["comment"].as_str().unwrap_or_default());
            }
        }
    }
    Ok(())
}

#[derive(Debug, clap::Args)]
pub struct QueryArgs {
    /// A PromQL expression
    pub query: String,
    #[arg(long, help = "Evaluate over this long back from now (e.g. 1h) instead of once")]
    pub range: Option<String>,
    #[arg(long, default_value = "1m", help = "Step of a range query")]
    pub step: String,
    #[arg(long, help = "Print the result as JSON (same as -o json)")]
    pub json: bool,
    #[arg(short = 'o', long, value_parser = ["text", "json"], help = "Output format: json prints the Prometheus API's data object")]
    pub output: Option<String>,
    #[arg(long, help = "kubeconfig context")]
    pub context: Option<String>,
}

pub async fn query(a: QueryArgs) -> Result<()> {
    let ep = endpoint(a.context.as_deref())?;
    let path = match &a.range {
        Some(r) => {
            let now = crate::obs::tsdb::now_ms() as f64 / 1000.0;
            let d = crate::obs::tsdb::parse_duration(r)?.as_secs_f64();
            format!("/prometheus/api/v1/query_range?query={}&start={}&end={now}&step={}", enc(&a.query), now - d, enc(&a.step))
        }
        None => format!("/prometheus/api/v1/query?query={}", enc(&a.query)),
    };
    let resp: hyper::Response<hyper::body::Incoming> = ep.send(Method::GET, &path, None, Vec::new()).await?;
    let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
    let v: Value = serde_json::from_slice(&body).context("not JSON")?;
    if v["status"] != "success" {
        bail!("{}", v["error"].as_str().unwrap_or("query failed"));
    }
    if a.json || a.output.as_deref() == Some("json") {
        println!("{}", serde_json::to_string_pretty(&v["data"])?);
        return Ok(());
    }
    let labels = |m: &Value| -> String {
        let o = m.as_object().cloned().unwrap_or_default();
        let name = o.get("__name__").and_then(Value::as_str).unwrap_or_default().to_string();
        let rest: Vec<String> = o.iter().filter(|(k, _)| *k != "__name__").map(|(k, v)| format!("{k}=\"{}\"", v.as_str().unwrap_or_default())).collect();
        format!("{name}{{{}}}", rest.join(", "))
    };
    match v["data"]["resultType"].as_str() {
        Some("vector") => {
            for r in v["data"]["result"].as_array().into_iter().flatten() {
                println!("{} {}", labels(&r["metric"]), r["value"][1].as_str().unwrap_or_default());
            }
        }
        Some("scalar") | Some("string") => println!("{}", v["data"]["result"][1].as_str().unwrap_or_default()),
        _ => {
            for r in v["data"]["result"].as_array().into_iter().flatten() {
                let vals: Vec<String> = r["values"].as_array().into_iter().flatten().map(|p| p[1].as_str().unwrap_or_default().to_string()).collect();
                println!("{} {}", labels(&r["metric"]), vals.join(" "));
            }
        }
    }
    Ok(())
}

/// `litek8s top` against a Kubernetes API server: nodes and pods from the
/// resource metrics API.
#[derive(Debug, Clone, clap::Args)]
pub struct TopArgs {
    #[arg(long, help = "kubeconfig context (default: the current one)")]
    pub context: Option<String>,
    #[arg(short = 'n', long, help = "Only pods of this namespace (default: all)")]
    pub namespace: Option<String>,
    #[arg(short = 'l', long, help = "Only pods matching this label selector")]
    pub selector: Option<String>,
    #[arg(long, help = "One row per container")]
    pub containers: bool,
    #[arg(long, default_value = "cpu", value_parser = ["cpu", "memory", "name"], help = "Order pods by cpu, memory or name")]
    pub sort_by: String,
    #[arg(long, help = "Print once, also on a terminal (piped output always prints once)")]
    pub once: bool,
}

fn milli(q: &Value) -> i128 {
    crate::kube::quantity::milli_of(q).unwrap_or(0)
}

/// `250m` (CPU), from thousandths of a core.
fn cores(m: i128) -> String {
    format!("{m}m")
}

/// `123Mi` (memory), from thousandths of a byte.
fn mebibytes(m: i128) -> String {
    format!("{}Mi", m / 1000 / (1 << 20))
}

fn percent(used: i128, of: i128) -> String {
    if of <= 0 { "-".into() } else { format!("{}%", used * 100 / of) }
}

/// Columns padded like kubectl's (three spaces apart).
fn columns(rows: &[Vec<String>]) -> String {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..n).map(|i| rows.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| if i + 1 == r.len() { c.clone() } else { format!("{c:<w$}", w = widths[i]) }).collect();
        out.push_str(line.join("   ").trim_end());
        out.push('\n');
    }
    out
}

/// One frame: nodes (usage against allocatable), then pods, busiest first.
pub async fn top_frame(ep: &Endpoint, a: &TopArgs) -> Result<String> {
    let nodes_metrics = ep.json(Method::GET, "/apis/metrics.k8s.io/v1beta1/nodes", None).await.map_err(|e| {
        anyhow::anyhow!("Metrics API not available on {} ({e:#}); litek8s serves it with observability on, or install metrics-server", ep.display)
    })?;
    let nodes = ep.json(Method::GET, "/api/v1/nodes", None).await?;
    let mut path = match &a.namespace {
        Some(ns) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{}/pods", enc(ns)),
        None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
    };
    if let Some(s) = &a.selector {
        path.push_str(&format!("?labelSelector={}", enc(s)));
    }
    let pods = ep.json(Method::GET, &path, None).await?;

    let usage: BTreeMap<&str, &Value> = nodes_metrics["items"].as_array().into_iter().flatten().filter_map(|m| Some((m["metadata"]["name"].as_str()?, &m["usage"]))).collect();
    let mut rows = vec![vec!["NODE".to_string(), "CPU".into(), "CPU%".into(), "MEMORY".into(), "MEMORY%".into()]];
    for n in nodes["items"].as_array().into_iter().flatten() {
        let name = n["metadata"]["name"].as_str().unwrap_or_default();
        let alloc = &n["status"]["allocatable"];
        match usage.get(name) {
            Some(u) => {
                let (cpu, mem) = (milli(&u["cpu"]), milli(&u["memory"]));
                rows.push(vec![name.to_string(), cores(cpu), percent(cpu, milli(&alloc["cpu"])), mebibytes(mem), percent(mem, milli(&alloc["memory"]))]);
            }
            None => rows.push(vec![name.to_string(), "<unknown>".into(), "<unknown>".into(), "<unknown>".into(), "<unknown>".into()]),
        }
    }
    let mut out = columns(&rows);
    out.push('\n');

    // (namespace, pod, container, cpu, memory)
    let mut items: Vec<(String, String, String, i128, i128)> = Vec::new();
    for p in pods["items"].as_array().into_iter().flatten() {
        let ns = p["metadata"]["namespace"].as_str().unwrap_or_default().to_string();
        let name = p["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let cs: Vec<&Value> = p["containers"].as_array().into_iter().flatten().collect();
        if a.containers {
            for c in cs {
                items.push((ns.clone(), name.clone(), c["name"].as_str().unwrap_or_default().to_string(), milli(&c["usage"]["cpu"]), milli(&c["usage"]["memory"])));
            }
        } else {
            let cpu = cs.iter().map(|c| milli(&c["usage"]["cpu"])).sum();
            let mem = cs.iter().map(|c| milli(&c["usage"]["memory"])).sum();
            items.push((ns, name, String::new(), cpu, mem));
        }
    }
    match a.sort_by.as_str() {
        "memory" => items.sort_by(|x, y| y.4.cmp(&x.4).then_with(|| (&x.0, &x.1, &x.2).cmp(&(&y.0, &y.1, &y.2)))),
        "name" => items.sort_by(|x, y| (&x.0, &x.1, &x.2).cmp(&(&y.0, &y.1, &y.2))),
        _ => items.sort_by(|x, y| y.3.cmp(&x.3).then_with(|| (&x.0, &x.1, &x.2).cmp(&(&y.0, &y.1, &y.2)))),
    }
    if items.is_empty() {
        out.push_str(&format!("No pod metrics{} yet.\n", a.namespace.as_ref().map(|n| format!(" in namespace {n}")).unwrap_or_default()));
        return Ok(out);
    }
    let mut header = vec!["NAMESPACE".to_string(), "POD".into()];
    if a.containers {
        header.push("CONTAINER".into());
    }
    header.extend(["CPU".to_string(), "MEMORY".into()]);
    let mut rows = vec![header];
    for (ns, pod, c, cpu, mem) in items {
        let mut r = vec![ns, pod];
        if a.containers {
            r.push(c);
        }
        r.extend([cores(cpu), mebibytes(mem)]);
        rows.push(r);
    }
    out.push_str(&columns(&rows));
    Ok(out)
}
