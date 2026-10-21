//! Relaying in the pod overlay, for node pairs with no direct path.
//!
//! The overlay is a full WireGuard mesh, which needs every pair of nodes to
//! have at least one side the other can reach. Two homes behind different
//! NATs, or a home node and a cloud VM without a public address, have none.
//! Such a pair goes through a relay: a node both can reach (by default the
//! servers, or nodes labeled `litek8s.dev/relay=true`; a cloud VM with a
//! public address is the typical one).
//!
//! The data path stays in the kernel: on each side the relay's peer gets the
//! other side's pod CIDR in its allowed IPs, the relay decrypts and its
//! routing table sends the packet back into `litek8s-wg` towards the other
//! node (forwarding between WireGuard peers is allowed only on relays, and
//! only between pod addresses). The direct peer stays configured, with no
//! allowed IPs, so its keepalives keep probing the direct path: when they
//! start getting through, the pair goes back to direct.
//!
//! Who reaches whom comes from the agents: each one probes and watches its
//! peers (answers received recently over a fresh session mean the path
//! works) and reports it; the server keeps the reports in the `litek8s.dev/wireguard-peers`
//! annotation of each Node, so every server of an HA control plane computes
//! the same routes from the same objects. This module is that computation:
//! pure functions over Node objects.
//!
//! A pair is relayed only when one of its sides reports the other
//! unreachable, never otherwise; both sides must see the path working again
//! to go back to direct. The relay of a pair is the same seen from both
//! sides (traffic and replies through one relay, or WireGuard drops the
//! replies), chosen by rendezvous hashing among the relays both sides
//! reach, so pairs spread over relays and stay put when one is added.
//!
//! The endpoints peers see a node at (its address behind NAT, as WireGuard
//! learned it from its packets) are kept in the reports too and offered to
//! the other nodes as extra endpoints to try: a node behind a router that
//! forwards the port, or maps it the same for every destination, is then
//! reached directly by the others.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What an agent observes of each peer (JSON map peer name → [`PeerReport`]),
/// written by the server from the agent's reports.
pub const ANNOTATION_PEERS: &str = "litek8s.dev/wireguard-peers";
/// The routes computed for a node (JSON map peer name → [`Route`] as
/// text), for people and the CLI; agents get theirs over their connection.
pub const ANNOTATION_ROUTES: &str = "litek8s.dev/wireguard-routes";
/// `true`: this node relays for others; `false`: never. Default: servers do.
pub const LABEL_RELAY: &str = "litek8s.dev/relay";
const CONTROL_PLANE_LABEL: &str = crate::kube::join::CONTROL_PLANE_LABEL;
/// Extra endpoints offered per node.
const MAX_OBSERVED: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// Packets arrive from the peer and the handshake is fresh.
    Direct,
    /// Nothing arrived from the peer for a while.
    Unreachable,
    /// Not watched long enough to tell (just configured).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReport {
    pub state: Reach,
    /// Where the peer's packets come from (host:port), when direct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

pub type Reports = BTreeMap<String, PeerReport>;

/// How a node reaches one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    Via(String),
    /// No direct path and no relay both sides reach: still tried directly.
    Unreachable,
}

impl Route {
    pub fn text(&self) -> String {
        match self {
            Route::Direct => "direct".into(),
            Route::Via(relay) => format!("via {relay}"),
            Route::Unreachable => "unreachable".into(),
        }
    }

    pub fn parse(text: &str) -> Route {
        match text.strip_prefix("via ") {
            Some(relay) => Route::Via(relay.to_string()),
            None if text == "unreachable" => Route::Unreachable,
            None => Route::Direct,
        }
    }
}

/// What the computation needs of one node of the overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshNode {
    pub name: String,
    /// Eligible to relay for others.
    pub relay: bool,
    pub server: bool,
    /// The endpoint it announces (host:port).
    pub endpoint: String,
    pub reports: Reports,
}

fn annotation<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.pointer("/metadata/annotations").and_then(|a| a.get(key)).and_then(Value::as_str)
}

fn label<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.pointer("/metadata/labels").and_then(|a| a.get(key)).and_then(Value::as_str)
}

pub fn reports_of(node: &Value) -> Reports {
    annotation(node, ANNOTATION_PEERS).and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default()
}

pub fn routes_of(node: &Value) -> BTreeMap<String, Route> {
    let text: BTreeMap<String, String> = annotation(node, ANNOTATION_ROUTES).and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default();
    text.into_iter().map(|(k, v)| (k, Route::parse(&v))).collect()
}

pub fn routes_text(routes: &BTreeMap<String, Route>) -> String {
    let text: BTreeMap<&String, String> = routes.iter().map(|(k, v)| (k, v.text())).collect();
    serde_json::to_string(&text).unwrap_or_default()
}

impl MeshNode {
    /// A Node object of the overlay (it announced a WireGuard endpoint).
    pub fn from_node(node: &Value) -> Option<MeshNode> {
        let endpoint = annotation(node, super::overlay::ANNOTATION_ENDPOINT)?.to_string();
        annotation(node, super::overlay::ANNOTATION_PUBLIC_KEY)?;
        let server = label(node, CONTROL_PLANE_LABEL).is_some();
        let relay = match label(node, LABEL_RELAY) {
            Some("true") => true,
            Some("false") => false,
            _ => server,
        };
        Some(MeshNode {
            name: node.pointer("/metadata/name").and_then(Value::as_str)?.to_string(),
            relay,
            server,
            endpoint,
            reports: reports_of(node),
        })
    }

    fn reports(&self, peer: &str) -> Option<Reach> {
        self.reports.get(peer).map(|r| r.state)
    }
}

/// Whether two nodes talk directly: neither side finds the other
/// unreachable.
pub fn pair_ok(a: &MeshNode, b: &MeshNode) -> bool {
    a.reports(&b.name) != Some(Reach::Unreachable) && b.reports(&a.name) != Some(Reach::Unreachable)
}

/// FNV-1a: stable across processes and versions (HA servers must agree).
fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The relay of a pair with no direct path: a relay node that both reach,
/// the same one whichever side asks.
pub fn relay_for<'a>(nodes: &'a [MeshNode], a: &MeshNode, b: &MeshNode) -> Option<&'a MeshNode> {
    let (lo, hi) = if a.name <= b.name { (&a.name, &b.name) } else { (&b.name, &a.name) };
    nodes
        .iter()
        .filter(|r| r.relay && r.name != a.name && r.name != b.name && pair_ok(a, r) && pair_ok(r, b))
        .max_by_key(|r| (fnv(&[lo, hi, &r.name]), std::cmp::Reverse(r.name.clone())))
}

/// How `me` reaches every other node.
pub fn routes_for(nodes: &[MeshNode], me: &str) -> BTreeMap<String, Route> {
    let Some(a) = nodes.iter().find(|n| n.name == me) else { return BTreeMap::new() };
    nodes
        .iter()
        .filter(|b| b.name != me)
        .map(|b| {
            let route = if pair_ok(a, b) {
                Route::Direct
            } else {
                match relay_for(nodes, a, b) {
                    Some(r) => Route::Via(r.name.clone()),
                    None => Route::Unreachable,
                }
            };
            (b.name.clone(), route)
        })
        .collect()
}

/// Where other nodes see `of`'s packets come from, besides the endpoint it
/// announces: the most seen first.
pub fn observed_endpoints(nodes: &[MeshNode], of: &str) -> Vec<String> {
    let Some(node) = nodes.iter().find(|n| n.name == of) else { return Vec::new() };
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.name != of) {
        if let Some(PeerReport { state: Reach::Direct, endpoint: Some(ep) }) = n.reports.get(of) {
            if *ep != node.endpoint {
                *seen.entry(ep).or_default() += 1;
            }
        }
    }
    let mut list: Vec<(&str, usize)> = seen.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    list.into_iter().take(MAX_OBSERVED).map(|(e, _)| e.to_string()).collect()
}

/// A node's new reports: what the agent observed now, keeping the last
/// known state of peers it cannot tell yet (it just restarted), dropping
/// peers it no longer has.
pub fn merge_reports(previous: &Reports, now: &Reports) -> Reports {
    now.iter()
        .filter_map(|(peer, r)| match r.state {
            Reach::Unknown => previous.get(peer).filter(|p| p.state != Reach::Unknown).map(|p| (peer.clone(), p.clone())),
            _ => Some((peer.clone(), r.clone())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, relay: bool, reports: &[(&str, Reach)]) -> MeshNode {
        MeshNode {
            name: name.into(),
            relay,
            server: relay,
            endpoint: format!("{name}:51820"),
            reports: reports.iter().map(|(p, s)| (p.to_string(), PeerReport { state: *s, endpoint: None })).collect(),
        }
    }

    use Reach::*;

    #[test]
    fn pairs_that_work_stay_direct() {
        let nodes = vec![node("s", true, &[("a", Direct), ("b", Direct)]), node("a", false, &[("s", Direct), ("b", Direct)]), node("b", false, &[])];
        for me in ["s", "a", "b"] {
            assert!(routes_for(&nodes, me).values().all(|r| *r == Route::Direct), "{me}");
        }
    }

    #[test]
    fn unknown_or_missing_reports_are_direct() {
        let nodes = vec![node("s", true, &[("a", Unknown)]), node("a", false, &[])];
        assert_eq!(routes_for(&nodes, "a")["s"], Route::Direct);
    }

    #[test]
    fn a_pair_without_a_path_goes_through_a_relay_both_reach() {
        // a and b behind different NATs; s public.
        let nodes = vec![
            node("s", true, &[("a", Direct), ("b", Direct)]),
            node("a", false, &[("s", Direct), ("b", Unreachable)]),
            node("b", false, &[("s", Direct), ("a", Direct)]),
        ];
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Via("s".into()));
        // One side's word is enough, and both sides agree on the relay.
        assert_eq!(routes_for(&nodes, "b")["a"], Route::Via("s".into()));
        assert_eq!(routes_for(&nodes, "a")["s"], Route::Direct);
        assert!(routes_for(&nodes, "s").values().all(|r| *r == Route::Direct));
    }

    #[test]
    fn only_eligible_relays_reached_by_both_sides_relay() {
        let mut nodes = vec![
            node("s", true, &[("a", Direct), ("b", Unreachable)]),
            node("r", false, &[]),
            node("a", false, &[("b", Unreachable)]),
            node("b", false, &[("a", Unreachable)]),
        ];
        // s does not reach b; r reaches both but does not relay.
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Unreachable);
        nodes[1].relay = true;
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Via("r".into()));
        assert_eq!(routes_for(&nodes, "b")["a"], Route::Via("r".into()));
        // A relay never relays for itself.
        assert_eq!(routes_for(&nodes, "s")["b"], Route::Via("r".into()));
    }

    #[test]
    fn back_to_direct_when_both_sides_see_the_path() {
        let mut nodes = vec![node("s", true, &[]), node("a", false, &[("b", Unreachable)]), node("b", false, &[("a", Direct)])];
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Via("s".into()));
        nodes[1].reports.insert("b".into(), PeerReport { state: Direct, endpoint: None });
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Direct);
        assert_eq!(routes_for(&nodes, "b")["a"], Route::Direct);
    }

    #[test]
    fn pairs_spread_over_relays_and_agree() {
        let mut nodes: Vec<MeshNode> = (0..3).map(|i| node(&format!("relay{i}"), true, &[])).collect();
        let homes: Vec<String> = (0..12).map(|i| format!("home{i}")).collect();
        for h in &homes {
            let unreachable: Vec<(&str, Reach)> = homes.iter().filter(|o| *o != h).map(|o| (o.as_str(), Unreachable)).collect();
            nodes.push(node(h, false, &unreachable));
        }
        let mut used = std::collections::BTreeSet::new();
        for a in &homes {
            for b in homes.iter().filter(|b| *b != a) {
                let Route::Via(r) = &routes_for(&nodes, a)[b] else { panic!() };
                assert_eq!(routes_for(&nodes, b)[a], Route::Via(r.clone()));
                used.insert(r.clone());
            }
        }
        assert_eq!(used.len(), 3, "{used:?}");
        // Adding a relay only moves the pairs it wins.
        let before: Vec<Route> = homes.iter().map(|b| routes_for(&nodes, &homes[0]).get(b).cloned().unwrap_or(Route::Direct)).collect();
        nodes.push(node("relay3", true, &[]));
        let after: Vec<Route> = homes.iter().map(|b| routes_for(&nodes, &homes[0]).get(b).cloned().unwrap_or(Route::Direct)).collect();
        for (x, y) in before.iter().zip(&after) {
            assert!(x == y || *y == Route::Via("relay3".into()));
        }
    }

    #[test]
    fn a_relay_that_is_down_is_not_used() {
        // s2 stopped: its own (old) reports say direct, the others' say not.
        let nodes = vec![
            node("s1", true, &[("a", Direct), ("b", Direct)]),
            node("s2", true, &[("a", Direct), ("b", Direct)]),
            node("a", false, &[("b", Unreachable), ("s2", Unreachable)]),
            node("b", false, &[("a", Unreachable), ("s2", Unreachable)]),
        ];
        assert_eq!(routes_for(&nodes, "a")["b"], Route::Via("s1".into()));
    }

    #[test]
    fn observed_endpoints_skip_the_announced_one() {
        let mut nodes = vec![node("s", true, &[]), node("a", false, &[]), node("b", false, &[])];
        let seen = |ep: &str| PeerReport { state: Direct, endpoint: Some(ep.into()) };
        nodes[0].reports.insert("a".into(), seen("203.0.113.7:40001"));
        nodes[2].reports.insert("a".into(), seen("a:51820"));
        nodes[2].reports.insert("s".into(), seen("198.51.100.1:51820"));
        assert_eq!(observed_endpoints(&nodes, "a"), vec!["203.0.113.7:40001".to_string()]);
        assert_eq!(observed_endpoints(&nodes, "s"), vec!["198.51.100.1:51820".to_string()]);
        assert!(observed_endpoints(&nodes, "b").is_empty());
        // Unreachable reports offer nothing.
        nodes[0].reports.insert("b".into(), PeerReport { state: Unreachable, endpoint: Some("x:1".into()) });
        assert!(observed_endpoints(&nodes, "b").is_empty());
    }

    #[test]
    fn merging_keeps_known_states_over_unknown() {
        let r = |s| PeerReport { state: s, endpoint: None };
        let previous: Reports = [("a".to_string(), r(Unreachable)), ("gone".to_string(), r(Direct))].into();
        let now: Reports = [("a".to_string(), r(Unknown)), ("b".to_string(), r(Unknown)), ("c".to_string(), r(Direct))].into();
        let merged = merge_reports(&previous, &now);
        assert_eq!(merged, [("a".to_string(), r(Unreachable)), ("c".to_string(), r(Direct))].into());
    }

    #[test]
    fn from_node_reads_labels_and_annotations() {
        let n = json!({"metadata": {"name": "s", "labels": {CONTROL_PLANE_LABEL: "true"}, "annotations": {
            super::super::overlay::ANNOTATION_ENDPOINT: "1.2.3.4:51820",
            super::super::overlay::ANNOTATION_PUBLIC_KEY: "k",
            ANNOTATION_PEERS: r#"{"a":{"state":"unreachable"}}"#,
        }}});
        let m = MeshNode::from_node(&n).unwrap();
        assert!(m.relay && m.server);
        assert_eq!(m.reports["a"].state, Unreachable);
        let mut opted_out = n.clone();
        opted_out["metadata"]["labels"][LABEL_RELAY] = json!("false");
        assert!(!MeshNode::from_node(&opted_out).unwrap().relay);
        let mut worker = n.clone();
        worker["metadata"]["labels"] = json!({LABEL_RELAY: "true"});
        let w = MeshNode::from_node(&worker).unwrap();
        assert!(w.relay && !w.server);
        assert!(MeshNode::from_node(&json!({"metadata": {"name": "x"}})).is_none());
        let routes: BTreeMap<String, Route> = [("a".to_string(), Route::Via("s".into())), ("b".to_string(), Route::Direct)].into();
        let mut annotated = n.clone();
        annotated["metadata"]["annotations"][ANNOTATION_ROUTES] = json!(routes_text(&routes));
        assert_eq!(routes_of(&annotated), routes);
    }
}
