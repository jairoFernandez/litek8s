//! `litek8s mcp`: a Model Context Protocol server on stdin/stdout, so an
//! LLM agent (Claude Code: `claude mcp add litek8s -- litek8s mcp`) can
//! operate litek8s clusters with tools that answer in structured JSON:
//! dev cluster lifecycle, get/describe with events and owners, apply and
//! delete, log search, PromQL, alerts, a "why is this pod not ready"
//! diagnosis and the dev cluster's Docker API. The docs and the agent skill
//! are its resources.
//!
//! Guardrails ([`guard`]): it only talks to litek8s API servers (`/version`
//! must say `+litek8s`, checked before any request with credentials) and
//! litek8s Docker engines; tools that delete take `confirm: true` and
//! otherwise say what they would do; `--read-only` leaves the mutating
//! tools out; results are cut to `--max-output` characters with a hint on
//! how to narrow them.
//!
//! JSON-RPC 2.0, one message per line (the MCP stdio transport). Nothing
//! else may write to stdout: dev cluster commands run as child processes
//! with their output captured.

pub mod diagnose;
pub mod guard;
pub mod resources;
pub mod tools;

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Protocol revisions this server speaks (newest first); it answers with the
/// client's when it knows it, else its newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Default `--max-output`: characters of one tool result.
pub const DEFAULT_MAX_OUTPUT: usize = 20_000;

#[derive(Debug, Clone, clap::Args)]
pub struct McpArgs {
    #[arg(long, help = "kubeconfig context to use when a tool call names none (default: like `litek8s ui`: $KUBECONFIG's current context, else the running dev cluster)")]
    pub context: Option<String>,
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT, help = "Most characters of one tool result; longer ones are cut with a hint")]
    pub max_output: usize,
    #[arg(long, help = "Leave out the tools that change anything (apply, delete, cluster up/down/delete, silences)")]
    pub read_only: bool,
}

const INSTRUCTIONS: &str = "litek8s tools: operate litek8s clusters (a Kubernetes API server in one Rust binary, with local dev clusters, built-in metrics/logs/alerts and a Docker API). \
They refuse kube contexts that are not litek8s (use kubectl for those). \
Start with clusters_list or cluster_status; pass `context` (litek8s-<cluster name>) to target a cluster. \
For a pod that is not Ready or crash-looping, call diagnose_pod first: one result with conditions, container states, last exit, probes, events and recent logs. \
Use logs_search for logs across pods (also of deleted containers) and query/query_range for PromQL (recipes in the resource litek8s://skill/references/observability.md). \
delete, cluster_down and cluster_delete need confirm: true; without it they report what they would do. \
Read litek8s://skill/SKILL.md for when to use litek8s vs kubectl and how it differs from upstream Kubernetes.";

pub async fn run(args: McpArgs) -> Result<()> {
    let server = Arc::new(tools::Server::new(args));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap_or_default();
            line.push(b'\n');
            if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(error_response(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        // Batches (2025-03-26) are answered message by message.
        let msgs = match msg {
            Value::Array(a) => a,
            m => vec![m],
        };
        for m in msgs {
            let server = server.clone();
            let tx = tx.clone();
            // Concurrently: a long `cluster_up` must not hold up a ping.
            tokio::spawn(async move {
                if let Some(resp) = handle(&server, m).await {
                    let _ = tx.send(resp);
                }
            });
        }
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One JSON-RPC message; the response for a request, `None` for a
/// notification (or a response from the client).
pub async fn handle(server: &tools::Server, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = match id {
        Some(id) if !method.is_empty() => id,
        // Notifications (initialized, cancelled) need no answer.
        _ => return None,
    };
    let result = match method.as_str() {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": server.list()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match server.call(name, &args).await {
                Some(r) => Ok(r),
                None => Err((-32602, format!("unknown tool {name:?}"))),
            }
        }
        "resources/list" => Ok(json!({"resources": resources::list()})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or_default();
            match resources::read(uri) {
                Some(text) => Ok(json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": text}]})),
                None => Err((-32002, format!("resource not found: {uri}"))),
            }
        }
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => error_response(id, code, &m),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": false}},
        "serverInfo": {"name": "litek8s", "title": "litek8s", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(read_only: bool) -> tools::Server {
        tools::Server::new(McpArgs { context: None, max_output: DEFAULT_MAX_OUTPUT, read_only })
    }

    #[tokio::test]
    async fn initialize_negotiates_the_protocol_version() {
        let s = server(false);
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "litek8s");
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}})).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(handle(&s, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).await.unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope", "arguments": {}}})).await.unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn resources_are_listed_and_readable() {
        let s = server(false);
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"})).await.unwrap();
        let uris: Vec<&str> = r["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
        assert!(uris.contains(&"litek8s://skill/SKILL.md"), "{uris:?}");
        assert!(uris.contains(&"litek8s://docs/observability.md"), "{uris:?}");
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "litek8s://docs/agents.md"}})).await.unwrap();
        assert!(r["result"]["contents"][0]["text"].as_str().unwrap().contains("litek8s mcp"));
        let r = handle(&s, json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": {"uri": "litek8s://docs/../../etc/passwd"}})).await.unwrap();
        assert!(r.get("error").is_some());
    }
}
