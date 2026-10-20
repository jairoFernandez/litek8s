//! The MCP server's guardrails: litek8s servers only, explicit confirmation
//! for deletes, results cut to a size an agent's context can take.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde_json::{Value, json};

use crate::ui::client::Endpoint;

/// Whether a `/version` answer is litek8s's (`v1.33.0+litek8s`).
pub fn is_litek8s_version(v: &Value) -> bool {
    v["gitVersion"].as_str().is_some_and(|g| g.split('+').skip(1).any(|meta| meta == "litek8s"))
}

/// Whether a Docker `/version` answer is litek8s's engine.
pub fn is_litek8s_docker(v: &Value) -> bool {
    v["Platform"]["Name"] == "litek8s"
}

/// Refuses an API server that is not litek8s. `/version` is asked without
/// credentials first, so a kubeconfig's exec plugin (cloud login) never
/// runs for a server that turns out not to be litek8s; static credentials
/// are tried when it insists on them.
pub async fn check_kube(ep: &Endpoint, context: &str) -> Result<Value> {
    let mut anon = ep.clone();
    anon.headers.clear();
    anon.creds = None;
    let ctx = if context.is_empty() { String::new() } else { format!("context {context} ") };
    let version = match tokio::time::timeout(Duration::from_secs(5), anon.json(Method::GET, "/version", None)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) if ep.creds.is_none() && !ep.headers.is_empty() && e.to_string().to_lowercase().contains("unauthorized") => {
            tokio::time::timeout(Duration::from_secs(5), ep.json(Method::GET, "/version", None)).await.context("timed out")??
        }
        Ok(Err(e)) => bail!("cannot reach {ctx}({}): {e:#}. Is the cluster running? (clusters_list, cluster_status, cluster_up)", ep.display),
        Err(_) => bail!("{ctx}({}) did not answer /version within 5s. Is the cluster running? (clusters_list, cluster_status)", ep.display),
    };
    if !is_litek8s_version(&version) {
        bail!(
            "refusing: {ctx}({}) is not a litek8s API server (gitVersion {}). These tools only operate litek8s clusters; use kubectl for other clusters.",
            ep.display,
            version["gitVersion"].as_str().unwrap_or("unknown")
        );
    }
    Ok(version)
}

pub async fn check_docker(ep: &Endpoint) -> Result<()> {
    let v = tokio::time::timeout(Duration::from_secs(5), ep.json(Method::GET, "/version", None))
        .await
        .context("the Docker API did not answer within 5s")?
        .with_context(|| format!("cannot reach the Docker API at {}. Is the dev cluster running? (cluster_status)", ep.display))?;
    if !is_litek8s_docker(&v) {
        bail!("refusing: {} is not litek8s's Docker engine (platform {}). These tools only talk to litek8s.", ep.display, v["Platform"]["Name"].as_str().unwrap_or("unknown"));
    }
    Ok(())
}

/// `confirm: true` given, or the answer to send instead: what would happen.
pub fn confirmed(args: &Value, would: Value) -> Result<(), Value> {
    if args.get("confirm").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let mut v = would;
    v["confirmed"] = json!(false);
    v["hint"] = json!("Nothing was changed. This is destructive: call again with confirm: true once the user agrees.");
    Err(v)
}

/// Arrays whose newest entries are at the end (keep the tail when cutting).
const TAIL_KEYS: &[&str] = &["lines", "logs", "previous_logs", "events", "values"];

/// Cuts a result to at most `max` characters of JSON: halves the biggest
/// array (or string) until it fits, and says so with `hint`.
pub fn fit(v: Value, max: usize, hint: &str) -> Value {
    let original = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
    if original <= max {
        return v;
    }
    let mut v = v;
    // Room for the note.
    let budget = max.saturating_sub(300 + hint.len());
    for _ in 0..200 {
        if serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0) <= budget || !shrink(&mut v) {
            break;
        }
    }
    let note = json!({"original_chars": original, "max_chars": max, "hint": hint});
    match &mut v {
        Value::Object(m) if serde_json::to_string(m).map(|s| s.len()).unwrap_or(0) <= budget => {
            m.insert("_truncated".into(), note);
            v
        }
        other => {
            let text = serde_json::to_string(other).unwrap_or_default();
            // Quotes double when the JSON is quoted again.
            let end = (0..=(budget / 2).min(text.len())).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
            json!({"partial": &text[..end], "_truncated": note})
        }
    }
}

/// Halves the largest array or string in `v`; false when nothing is left
/// to cut.
fn shrink(v: &mut Value) -> bool {
    fn size(v: &Value) -> usize {
        serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
    }
    // Find the path of the biggest cuttable node.
    fn best<'a>(v: &'a Value, key: &str, path: &mut Vec<String>, found: &mut Option<(usize, Vec<String>, bool)>) {
        let cuttable = match v {
            Value::Array(a) => a.len() > 1,
            Value::String(s) => s.len() > 400,
            _ => false,
        };
        if cuttable {
            let s = size(v);
            if found.as_ref().is_none_or(|(b, _, _)| s > *b) {
                *found = Some((s, path.clone(), TAIL_KEYS.contains(&key)));
            }
        }
        match v {
            Value::Object(m) => {
                for (k, c) in m {
                    path.push(k.clone());
                    best(c, k, path, found);
                    path.pop();
                }
            }
            Value::Array(a) => {
                for (i, c) in a.iter().enumerate() {
                    path.push(i.to_string());
                    best(c, key, path, found);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut found = None;
    best(v, "", &mut Vec::new(), &mut found);
    let Some((_, path, tail)) = found else { return false };
    let mut node = v;
    for p in &path {
        node = match node {
            Value::Object(m) => m.get_mut(p).unwrap(),
            Value::Array(a) => &mut a[p.parse::<usize>().unwrap()],
            _ => return false,
        };
    }
    match node {
        Value::Array(a) => {
            let keep = a.len() / 2;
            if tail {
                a.drain(..a.len() - keep);
            } else {
                a.truncate(keep);
            }
        }
        Value::String(s) => {
            let mut end = s.len() / 2;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            s.push_str("…[cut]");
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_litek8s_from_other_servers() {
        assert!(is_litek8s_version(&json!({"gitVersion": "v1.33.0+litek8s"})));
        assert!(!is_litek8s_version(&json!({"gitVersion": "v1.33.4+k3s1"})));
        assert!(!is_litek8s_version(&json!({"gitVersion": "v1.31.5-gke.1023000"})));
        assert!(!is_litek8s_version(&json!({"gitVersion": "v1.33.0-litek8s"})));
        assert!(!is_litek8s_version(&json!({"gitVersion": "v1.33.0+notlitek8s"})));
        assert!(!is_litek8s_version(&json!({})));
        assert!(is_litek8s_version(&json!({"gitVersion": crate::kube::discovery::KUBE_GIT_VERSION})));
        assert!(is_litek8s_docker(&json!({"Platform": {"Name": "litek8s"}})));
        assert!(!is_litek8s_docker(&json!({"Platform": {"Name": "Docker Engine - Community"}})));
    }

    #[test]
    fn destructive_calls_need_confirm() {
        let would = json!({"would_delete": "pod/x"});
        let r = confirmed(&json!({}), would.clone()).unwrap_err();
        assert_eq!(r["confirmed"], false);
        assert_eq!(r["would_delete"], "pod/x");
        assert!(confirmed(&json!({"confirm": "true"}), would.clone()).is_err(), "a string is not a yes");
        assert!(confirmed(&json!({"confirm": false}), would.clone()).is_err());
        assert!(confirmed(&json!({"confirm": true}), would).is_ok());
    }

    #[test]
    fn cuts_results_to_size_and_says_how_to_narrow() {
        let small = json!({"a": 1});
        assert_eq!(fit(small.clone(), 1000, "x"), small);
        let rows: Vec<Value> = (0..2000).map(|i| json!({"name": format!("pod-{i}"), "status": "Running"})).collect();
        let lines: Vec<String> = (0..2000).map(|i| format!("line {i}")).collect();
        let v = fit(json!({"items": rows, "lines": lines}), 5000, "use a label selector");
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.len() <= 5000, "{}", s.len());
        assert_eq!(v["_truncated"]["hint"], "use a label selector");
        // Lists keep their head, logs their tail (the newest lines).
        assert_eq!(v["items"][0]["name"], "pod-0");
        assert_eq!(v["lines"].as_array().unwrap().last().unwrap(), "line 1999");
        let big = json!({"text": "é".repeat(50_000)});
        let v = fit(big, 3000, "h");
        assert!(serde_json::to_string(&v).unwrap().len() <= 3000);
        let v = fit(json!("x".repeat(10_000)), 2000, "h");
        assert!(serde_json::to_string(&v).unwrap().len() <= 2000);
        assert!(v["_truncated"].is_object());
    }
}
