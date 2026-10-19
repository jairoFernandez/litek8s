//! A linearizability checker for a key-value workload on the API, and the
//! workload itself: Jepsen-lite for the HA control plane.
//!
//! Clients create, overwrite, delete and read ConfigMaps (`lin-<key>`, the
//! value in `data.v`) through any server while servers are killed or
//! partitioned; every operation is recorded with its call and return time.
//! An operation that failed in a way that leaves its effect unknown (a
//! timeout, a 5xx, a dropped connection) is recorded as *indeterminate*: it
//! may have taken effect at any point after its call, or never.
//!
//! The checker is the Wing & Gong / Lowe algorithm as in porcupine:
//! depth-first search for a linearization, memoizing (linearized set,
//! state) pairs, run per key (linearizability is compositional).

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    Read,
    Create(i64),
    Write(i64),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Output {
    /// A read's result.
    Value(Option<i64>),
    Ok,
    AlreadyExists,
    NotFound,
    /// Unknown whether it happened.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub client: u32,
    pub key: u32,
    pub input: Input,
    pub output: Output,
    /// Nanoseconds since the start of the run.
    pub call: u64,
    /// `u64::MAX` when the outcome is unknown.
    pub ret: u64,
}

type State = Option<i64>;

/// The register's sequential specification: the state after `op`, or
/// `None` when `op` could not have answered what it did in `state`.
fn step(state: State, op: &Op) -> Option<State> {
    match (op.input, op.output) {
        (Input::Read, Output::Value(v)) => (v == state).then_some(state),
        (Input::Read, _) => Some(state),
        (Input::Create(v), Output::Ok) => state.is_none().then_some(Some(v)),
        (Input::Create(_), Output::AlreadyExists) => state.is_some().then_some(state),
        (Input::Create(v), Output::Unknown) => Some(if state.is_none() { Some(v) } else { state }),
        (Input::Write(v), Output::Ok) => state.is_some().then_some(Some(v)),
        (Input::Write(_), Output::NotFound) => state.is_none().then_some(state),
        (Input::Write(v), Output::Unknown) => Some(state.map(|_| v)),
        (Input::Delete, Output::Ok) => state.is_some().then_some(None),
        (Input::Delete, Output::NotFound) => state.is_none().then_some(state),
        (Input::Delete, Output::Unknown) => Some(None),
        _ => None,
    }
}

struct Node {
    prev: usize,
    next: usize,
    call: bool,
    op: usize,
    /// For a call: its return node.
    ret: usize,
}

const HEAD: usize = 0;
const NIL: usize = usize::MAX;

/// Check one key's history. `Ok` or the longest linearizable prefix found.
fn check_single(ops: &[&Op]) -> Result<(), String> {
    let mut events: Vec<(u64, u8, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        events.push((op.call, 0, i));
        events.push((op.ret, 1, i));
    }
    events.sort();
    // Node 0 is the list head.
    let mut nodes = vec![Node { prev: NIL, next: NIL, call: false, op: 0, ret: NIL }];
    let mut ret_node = vec![NIL; ops.len()];
    let mut call_node = vec![NIL; ops.len()];
    for (_, kind, i) in &events {
        let idx = nodes.len();
        nodes.push(Node { prev: idx - 1, next: NIL, call: *kind == 0, op: *i, ret: NIL });
        nodes[idx - 1].next = idx;
        if *kind == 0 {
            call_node[*i] = idx;
        } else {
            ret_node[*i] = idx;
        }
    }
    for i in 0..ops.len() {
        nodes[call_node[i]].ret = ret_node[i];
    }
    let words = ops.len().div_ceil(64);
    let mut linearized = vec![0u64; words];
    let mut cache: HashSet<(Vec<u64>, State)> = HashSet::new();
    let mut calls: Vec<(usize, State)> = Vec::new();
    let mut state: State = None;
    let mut entry = nodes[HEAD].next;
    let mut best = 0usize;
    let lift = |nodes: &mut Vec<Node>, n: usize| {
        let (p, x) = (nodes[n].prev, nodes[n].next);
        nodes[p].next = x;
        if x != NIL {
            nodes[x].prev = p;
        }
        let r = nodes[n].ret;
        let (p, x) = (nodes[r].prev, nodes[r].next);
        nodes[p].next = x;
        if x != NIL {
            nodes[x].prev = p;
        }
    };
    let unlift = |nodes: &mut Vec<Node>, n: usize| {
        let r = nodes[n].ret;
        let (p, x) = (nodes[r].prev, nodes[r].next);
        nodes[p].next = r;
        if x != NIL {
            nodes[x].prev = r;
        }
        let (p, x) = (nodes[n].prev, nodes[n].next);
        nodes[p].next = n;
        if x != NIL {
            nodes[x].prev = n;
        }
    };
    let mut steps: u64 = 0;
    while nodes[HEAD].next != NIL {
        steps += 1;
        if steps > 50_000_000 {
            return Err(format!("search too large ({} ops); inconclusive", ops.len()));
        }
        if entry != NIL && nodes[entry].call {
            let i = nodes[entry].op;
            if let Some(next) = step(state, ops[i]) {
                let mut lin = linearized.clone();
                lin[i / 64] |= 1 << (i % 64);
                if cache.insert((lin.clone(), next)) {
                    calls.push((entry, state));
                    state = next;
                    linearized = lin;
                    lift(&mut nodes, entry);
                    best = best.max(calls.len());
                    entry = nodes[HEAD].next;
                    continue;
                }
            }
            entry = nodes[entry].next;
        } else {
            // A return reached whose call is not linearized: backtrack.
            let Some((n, old)) = calls.pop() else {
                return Err(format!("not linearizable: at most {best} of {} operations linearize", ops.len()));
            };
            let i = nodes[n].op;
            state = old;
            linearized[i / 64] &= !(1 << (i % 64));
            unlift(&mut nodes, n);
            entry = nodes[n].next;
        }
    }
    Ok(())
}

/// Check a whole history, key by key.
pub fn check(ops: &[Op]) -> Result<(), String> {
    let mut by_key: BTreeMap<u32, Vec<&Op>> = BTreeMap::new();
    for op in ops {
        by_key.entry(op.key).or_default().push(op);
    }
    for (key, ops) in by_key {
        check_single(&ops).map_err(|e| format!("key {key}: {e}"))?;
    }
    Ok(())
}

/// Summary of a run.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub ops: usize,
    pub ok: usize,
    pub failed: usize,
    pub unknown: usize,
    pub reads: usize,
}

pub fn summarize(ops: &[Op]) -> Summary {
    let mut s = Summary { ops: ops.len(), ..Default::default() };
    for op in ops {
        match op.output {
            Output::Unknown => s.unknown += 1,
            Output::AlreadyExists | Output::NotFound => s.failed += 1,
            Output::Value(_) => s.reads += 1,
            Output::Ok => s.ok += 1,
        }
    }
    s
}

// ---------------------------------------------------------------------------
// The workload

/// One HTTP/1.1 request on a fresh connection, with a deadline.
pub async fn http(addr: &str, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<(u16, Value), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let run = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed response")?;
        let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or("no status")?;
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_string() };
        Ok((status, serde_json::from_str(&body).unwrap_or(Value::String(body))))
    };
    tokio::time::timeout(timeout, run).await.map_err(|_| "timed out".to_string())?
}

pub fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail.get(n + 2..).unwrap_or_default();
    }
    out
}

/// Workload settings.
#[derive(Debug, Clone)]
pub struct Workload {
    pub servers: Vec<String>,
    pub clients: u32,
    pub keys: u32,
    pub duration: Duration,
    pub timeout: Duration,
    pub namespace: String,
}

fn cm(name: &str, v: i64) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}, "data": {"v": v.to_string()}})
}

/// Run the workload; returns the recorded history.
pub async fn run(w: Workload) -> Vec<Op> {
    let start = Instant::now();
    let mut tasks = Vec::new();
    for client in 0..w.clients {
        let w = w.clone();
        tasks.push(tokio::spawn(async move {
            let mut ops = Vec::new();
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ (u64::from(client) + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let mut rnd = move || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut counter: i64 = 0;
            while start.elapsed() < w.duration {
                let key = (rnd() % u64::from(w.keys)) as u32;
                // Each client mostly talks to one server, sometimes another.
                let server = &w.servers[((u64::from(client) + if rnd() % 4 == 0 { rnd() } else { 0 }) % w.servers.len() as u64) as usize];
                counter += 1;
                let value = i64::from(client) * 1_000_000 + counter;
                let name = format!("lin-{key}");
                let base = format!("/api/v1/namespaces/{}/configmaps", w.namespace);
                let input = match rnd() % 10 {
                    0..=3 => Input::Read,
                    4..=5 => Input::Write(value),
                    6..=7 => Input::Create(value),
                    _ => Input::Delete,
                };
                let call = start.elapsed().as_nanos() as u64;
                let result = match input {
                    Input::Read => http(server, "GET", &format!("{base}/{name}"), None, w.timeout).await,
                    Input::Create(v) => http(server, "POST", &base, Some(&cm(&name, v)), w.timeout).await,
                    Input::Write(v) => http(server, "PUT", &format!("{base}/{name}"), Some(&cm(&name, v)), w.timeout).await,
                    Input::Delete => http(server, "DELETE", &format!("{base}/{name}"), None, w.timeout).await,
                };
                let ret = start.elapsed().as_nanos() as u64;
                let output = match (input, &result) {
                    (Input::Read, Ok((200, body))) => Output::Value(body.pointer("/data/v").and_then(Value::as_str).and_then(|v| v.parse().ok())),
                    (Input::Read, Ok((404, _))) => Output::Value(None),
                    // A failed read tells nothing.
                    (Input::Read, _) => continue,
                    (Input::Create(_), Ok((201 | 200, _))) | (Input::Write(_), Ok((200, _))) | (Input::Delete, Ok((200 | 202, _))) => Output::Ok,
                    (Input::Create(_), Ok((409, _))) => Output::AlreadyExists,
                    (Input::Write(_) | Input::Delete, Ok((404, _))) => Output::NotFound,
                    _ => Output::Unknown,
                };
                let ret = if output == Output::Unknown { u64::MAX } else { ret };
                ops.push(Op { client, key, input, output, call, ret });
                if output == Output::Unknown {
                    // Back off a little while the cluster recovers.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            ops
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        if let Ok(ops) = t.await {
            all.extend(ops);
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(key: u32, input: Input, output: Output, call: u64, ret: u64) -> Op {
        Op { client: 0, key, input, output, call, ret }
    }

    #[test]
    fn sequential_history_is_linearizable() {
        let h = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Read, Output::Value(Some(1)), 2, 3),
            op(0, Input::Write(2), Output::Ok, 4, 5),
            op(0, Input::Create(3), Output::AlreadyExists, 6, 7),
            op(0, Input::Delete, Output::Ok, 8, 9),
            op(0, Input::Read, Output::Value(None), 10, 11),
        ];
        assert!(check(&h).is_ok());
    }

    #[test]
    fn stale_read_is_caught() {
        let h = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Write(2), Output::Ok, 2, 3),
            // Starts after the write returned, yet sees the old value.
            op(0, Input::Read, Output::Value(Some(1)), 4, 5),
        ];
        assert!(check(&h).is_err());
    }

    #[test]
    fn concurrent_operations_may_reorder() {
        let h = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Write(2), Output::Ok, 2, 10),
            op(0, Input::Read, Output::Value(Some(1)), 3, 4),
            op(0, Input::Read, Output::Value(Some(2)), 5, 6),
            // Once a read saw 2, a later read cannot see 1 again.
            op(0, Input::Read, Output::Value(Some(1)), 7, 8),
        ];
        assert!(check(&h).is_err());
        assert!(check(&h[..4]).is_ok());
    }

    #[test]
    fn indeterminate_writes_may_or_may_not_happen() {
        let happened = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Write(2), Output::Unknown, 2, u64::MAX),
            op(0, Input::Read, Output::Value(Some(2)), 10, 11),
        ];
        assert!(check(&happened).is_ok());
        let not_happened = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Write(2), Output::Unknown, 2, u64::MAX),
            op(0, Input::Read, Output::Value(Some(1)), 10, 11),
        ];
        assert!(check(&not_happened).is_ok());
        // A lost acknowledged write is not linearizable.
        let lost = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Write(2), Output::Ok, 2, 3),
            op(0, Input::Read, Output::Value(Some(1)), 10, 11),
        ];
        assert!(check(&lost).is_err());
        // A duplicated create (applied twice) shows as AlreadyExists for a
        // create nobody else made.
        let dup = vec![
            op(0, Input::Create(1), Output::Ok, 0, 1),
            op(0, Input::Delete, Output::Ok, 2, 3),
            op(0, Input::Create(5), Output::AlreadyExists, 4, 5),
        ];
        assert!(check(&dup).is_err());
    }
}
