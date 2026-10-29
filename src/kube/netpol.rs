//! NetworkPolicy → the rules of one pod, computed by the API server for the
//! node agents (`crate::kube::agent::netpol` enforces them).
//!
//! Everything here is pure: a [`Snapshot`] of the cluster's policies,
//! namespaces and pods answers, for a pod, whether it is isolated for
//! ingress and/or egress and which (peer addresses, ports) are allowed.
//! Selectors become addresses here (pod IPs, `ipBlock` CIDRs with their
//! `except` ranges cut out), named ports become numbers (against the target
//! pod for ingress, against each peer pod for egress), so the agent only
//! renders iptables rules.
//!
//! The cluster DNS is served by the API server process, not a pod: a
//! policy peer that would select CoreDNS (namespace `kube-system`, labels
//! `k8s-app: kube-dns`, ports `dns`/`dns-tcp` 53) allows it, which the
//! agent turns into the DNS address the pods' resolv.conf names.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::Ipv4Addr;

use serde_json::{Value, json};

use super::agent::netpol::{Allow, PodRules, PortRange};
use super::controllers::util;
use super::selector::LabelSelector;

pub const GROUP: &str = "networking.k8s.io";
pub const RESOURCE: &str = "networkpolicies";

/// A pod as a policy peer.
struct Peer {
    namespace: String,
    object: Value,
    /// `None`: the cluster DNS (no address of its own).
    ip: Option<Ipv4Addr>,
}

impl Peer {
    fn cidr(&self) -> Option<String> {
        self.ip.map(|ip| format!("{ip}/32"))
    }
}

/// What the rules of every pod are computed from.
pub struct Snapshot {
    /// namespace → its policies.
    policies: HashMap<String, Vec<Value>>,
    /// namespace name → the Namespace (with `kubernetes.io/metadata.name`).
    namespaces: HashMap<String, Value>,
    peers: Vec<Peer>,
}

impl Snapshot {
    pub fn new(policies: Vec<Value>, namespaces: Vec<Value>, pods: &[Value]) -> Self {
        let mut by_ns: HashMap<String, Vec<Value>> = HashMap::new();
        for p in policies {
            by_ns.entry(util::s(&p, "/metadata/namespace").to_string()).or_default().push(p);
        }
        let mut ns_map: HashMap<String, Value> = HashMap::new();
        for mut n in namespaces {
            let name = util::name(&n);
            if !n["metadata"]["labels"].is_object() {
                n["metadata"]["labels"] = json!({});
            }
            n["metadata"]["labels"]["kubernetes.io/metadata.name"] = json!(name);
            ns_map.insert(name, n);
        }
        // kube-system always exists for the virtual DNS pod.
        ns_map.entry("kube-system".into()).or_insert_with(|| json!({"metadata": {"name": "kube-system", "labels": {"kubernetes.io/metadata.name": "kube-system"}}}));
        let mut peers: Vec<Peer> = pods
            .iter()
            .filter(|p| !host_network(p) && !util::is_terminal(p))
            .filter_map(|p| {
                let ip: Ipv4Addr = util::s(p, "/status/podIP").parse().ok()?;
                Some(Peer { namespace: util::s(p, "/metadata/namespace").to_string(), object: p.clone(), ip: Some(ip) })
            })
            .collect();
        peers.push(Peer { namespace: "kube-system".into(), object: dns_pod(), ip: None });
        Self { policies: by_ns, namespaces: ns_map, peers }
    }

    pub fn has_policies(&self) -> bool {
        self.policies.values().any(|v| !v.is_empty())
    }

    /// The rules of `pod`; `None` when no policy selects it (not isolated
    /// in either direction).
    pub fn rules_for(&self, pod: &Value) -> Option<PodRules> {
        if host_network(pod) {
            return None;
        }
        let ns = util::s(pod, "/metadata/namespace");
        let mut out = PodRules::default();
        for policy in self.policies.get(ns).into_iter().flatten() {
            let selects = LabelSelector::from_object(Some(policy.pointer("/spec/podSelector").unwrap_or(&json!({})))).is_some_and(|s| s.matches(pod));
            if !selects {
                continue;
            }
            let (ingress, egress) = policy_types(policy);
            if ingress {
                let rules = out.ingress.get_or_insert_with(Vec::new);
                for rule in policy.pointer("/spec/ingress").and_then(Value::as_array).into_iter().flatten() {
                    rules.extend(self.ingress_rule(ns, rule, pod));
                }
            }
            if egress {
                let rules = out.egress.get_or_insert_with(Vec::new);
                for rule in policy.pointer("/spec/egress").and_then(Value::as_array).into_iter().flatten() {
                    rules.extend(self.egress_rule(ns, rule));
                }
            }
        }
        if out.ingress.is_none() && out.egress.is_none() {
            return None;
        }
        for list in [&mut out.ingress, &mut out.egress].into_iter().flatten() {
            list.sort();
            list.dedup();
        }
        Some(out)
    }

    /// The peers of a rule (`from` or `to`): `None` for any address, else
    /// the matching pods and the ipBlock CIDRs.
    fn peers<'a>(&'a self, policy_ns: &str, list: Option<&Value>) -> Option<(Vec<&'a Peer>, BTreeSet<String>)> {
        let list = list.and_then(Value::as_array).filter(|l| !l.is_empty())?;
        let mut pods: Vec<&Peer> = Vec::new();
        let mut cidrs = BTreeSet::new();
        for peer in list {
            if let Some(block) = peer.get("ipBlock").filter(|b| b.is_object()) {
                let except: Vec<&str> = block.get("except").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                cidrs.extend(subtract(util::s(block, "/cidr"), &except));
                continue;
            }
            let pod_sel = match peer.get("podSelector").filter(|s| !s.is_null()) {
                Some(s) => match LabelSelector::from_object(Some(s)) {
                    Some(sel) => Some(sel),
                    None => continue,
                },
                None => None,
            };
            let ns_sel = match peer.get("namespaceSelector").filter(|s| !s.is_null()) {
                Some(s) => match LabelSelector::from_object(Some(s)) {
                    Some(sel) => Some(sel),
                    None => continue,
                },
                None => None,
            };
            if pod_sel.is_none() && ns_sel.is_none() {
                continue;
            }
            for p in &self.peers {
                let ns_ok = match &ns_sel {
                    // Without a namespaceSelector: the policy's namespace.
                    None => p.namespace == policy_ns,
                    Some(sel) => self.namespaces.get(&p.namespace).is_some_and(|n| sel.matches(n)),
                };
                if ns_ok && pod_sel.as_ref().is_none_or(|sel| sel.matches(&p.object)) && !pods.iter().any(|q| std::ptr::eq(*q, p)) {
                    pods.push(p);
                }
            }
        }
        Some((pods, cidrs))
    }

    fn ingress_rule(&self, ns: &str, rule: &Value, target: &Value) -> Option<Allow> {
        let ports = match rule.get("ports").and_then(Value::as_array).filter(|p| !p.is_empty()) {
            None => Vec::new(),
            Some(list) => {
                let resolved: Vec<PortRange> = list.iter().filter_map(|p| port_spec(p).resolve(target)).collect();
                if resolved.is_empty() {
                    // Only named ports the pod does not have: allows nothing.
                    return None;
                }
                resolved
            }
        };
        let peers = match self.peers(ns, rule.get("from")) {
            None => None,
            Some((pods, mut cidrs)) => {
                cidrs.extend(pods.iter().filter_map(|p| p.cidr()));
                if cidrs.is_empty() {
                    return None;
                }
                Some(cidrs.into_iter().collect())
            }
        };
        Some(Allow { peers, dns: false, ports })
    }

    fn egress_rule(&self, ns: &str, rule: &Value) -> Vec<Allow> {
        let specs: Vec<PortSpec> = rule.get("ports").and_then(Value::as_array).into_iter().flatten().map(port_spec).collect();
        let peers = self.peers(ns, rule.get("to"));
        let mut out = Vec::new();
        // Peers as addresses (for numbered ports): None = any.
        let (addrs, dns) = match &peers {
            None => (None, false),
            Some((pods, cidrs)) => {
                let mut all = cidrs.clone();
                all.extend(pods.iter().filter_map(|p| p.cidr()));
                (Some(all.into_iter().collect::<Vec<_>>()), pods.iter().any(|p| p.ip.is_none()))
            }
        };
        let has_peers = addrs.as_ref().is_none_or(|a| !a.is_empty()) || dns;
        if specs.is_empty() {
            if has_peers {
                out.push(Allow { peers: addrs, dns, ports: Vec::new() });
            }
            return out;
        }
        let numbered: Vec<PortRange> = specs.iter().filter_map(|s| s.numbered()).collect();
        if !numbered.is_empty() && has_peers {
            out.push(Allow { peers: addrs.clone(), dns, ports: numbered });
        }
        // Named ports: what each destination pod calls that name.
        let named: Vec<&PortSpec> = specs.iter().filter(|s| matches!(s.port, PortRef::Name(_))).collect();
        if named.is_empty() {
            return out;
        }
        let candidates: Vec<&Peer> = match &peers {
            None => self.peers.iter().collect(),
            Some((pods, _)) => pods.clone(),
        };
        let mut grouped: BTreeMap<PortRange, (BTreeSet<String>, bool)> = BTreeMap::new();
        for spec in named {
            for p in &candidates {
                if let Some(range) = spec.resolve(&p.object) {
                    let entry = grouped.entry(range).or_default();
                    match p.cidr() {
                        Some(c) => {
                            entry.0.insert(c);
                        }
                        None => entry.1 = true,
                    }
                }
            }
        }
        for (range, (cidrs, dns)) in grouped {
            out.push(Allow { peers: Some(cidrs.into_iter().collect()), dns, ports: vec![range] });
        }
        out
    }
}

fn host_network(pod: &Value) -> bool {
    pod.pointer("/spec/hostNetwork").and_then(Value::as_bool).unwrap_or(false)
}

/// The cluster DNS as the CoreDNS pod a policy would select.
fn dns_pod() -> Value {
    json!({
        "metadata": {"namespace": "kube-system", "labels": {"k8s-app": "kube-dns"}},
        "spec": {"containers": [{"ports": [
            {"name": "dns", "containerPort": 53, "protocol": "UDP"},
            {"name": "dns-tcp", "containerPort": 53, "protocol": "TCP"},
        ]}]},
    })
}

/// (ingress, egress) as `policyTypes` says, or as Kubernetes defaults it:
/// Ingress always, Egress when there are egress rules.
fn policy_types(policy: &Value) -> (bool, bool) {
    match policy.pointer("/spec/policyTypes").and_then(Value::as_array).filter(|t| !t.is_empty()) {
        Some(types) => (types.iter().any(|t| t == "Ingress"), types.iter().any(|t| t == "Egress")),
        None => (true, policy.pointer("/spec/egress").and_then(Value::as_array).is_some_and(|e| !e.is_empty())),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum PortRef {
    Any,
    Number(u16, Option<u16>),
    Name(String),
}

#[derive(Debug, Clone)]
struct PortSpec {
    protocol: String,
    port: PortRef,
}

fn port_spec(p: &Value) -> PortSpec {
    let protocol = match util::s(p, "/protocol") {
        "" => "tcp".to_string(),
        other => other.to_lowercase(),
    };
    let end = p.get("endPort").and_then(Value::as_u64).and_then(|e| u16::try_from(e).ok());
    let port = match p.get("port") {
        Some(Value::Number(n)) => n.as_u64().and_then(|n| u16::try_from(n).ok()).map_or(PortRef::Any, |n| PortRef::Number(n, end)),
        Some(Value::String(s)) => match s.parse::<u16>() {
            Ok(n) => PortRef::Number(n, end),
            Err(_) => PortRef::Name(s.clone()),
        },
        _ => PortRef::Any,
    };
    PortSpec { protocol, port }
}

impl PortSpec {
    fn numbered(&self) -> Option<PortRange> {
        match &self.port {
            PortRef::Any => Some(PortRange { protocol: self.protocol.clone(), port: None }),
            PortRef::Number(n, end) => Some(PortRange { protocol: self.protocol.clone(), port: Some((*n, end.filter(|e| e > n).unwrap_or(*n))) }),
            PortRef::Name(_) => None,
        }
    }

    /// The port on `pod`: numbers as they are, a name looked up in its
    /// containers' ports (same protocol).
    fn resolve(&self, pod: &Value) -> Option<PortRange> {
        let PortRef::Name(name) = &self.port else { return self.numbered() };
        let n = pod
            .pointer("/spec/containers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|c| c.get("ports").and_then(Value::as_array).cloned().unwrap_or_default())
            .find(|p| util::s(p, "/name") == name && protocol_of(p) == self.protocol)?
            .get("containerPort")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())?;
        Some(PortRange { protocol: self.protocol.clone(), port: Some((n, n)) })
    }
}

/// A container port's protocol, lower case (TCP when unset).
fn protocol_of(p: &Value) -> String {
    match util::s(p, "/protocol") {
        "" => "tcp".to_string(),
        o => o.to_lowercase(),
    }
}

fn parse_cidr(cidr: &str) -> Option<(u32, u8)> {
    let (addr, prefix) = match cidr.split_once('/') {
        Some((a, p)) => (a, p.parse::<u8>().ok()?),
        None => (cidr, 32),
    };
    let addr: Ipv4Addr = addr.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((u32::from(addr) & mask, prefix))
}

fn contains(net: (u32, u8), other: (u32, u8)) -> bool {
    if other.1 < net.1 {
        return false;
    }
    let mask = if net.1 == 0 { 0 } else { u32::MAX << (32 - net.1) };
    other.0 & mask == net.0
}

/// `cidr` without the `except` ranges, as CIDRs (IPv4 only: the pod
/// network has no IPv6, so IPv6 blocks match nothing).
pub fn subtract(cidr: &str, except: &[&str]) -> Vec<String> {
    let Some(net) = parse_cidr(cidr) else { return Vec::new() };
    let except: Vec<(u32, u8)> = except.iter().filter_map(|e| parse_cidr(e)).collect();
    let mut out = Vec::new();
    fn walk(net: (u32, u8), except: &[(u32, u8)], out: &mut Vec<String>) {
        if except.iter().any(|e| contains(*e, net)) {
            return;
        }
        if !except.iter().any(|e| contains(net, *e)) || net.1 == 32 {
            out.push(format!("{}/{}", Ipv4Addr::from(net.0), net.1));
            return;
        }
        let half = 1u32 << (31 - net.1);
        walk((net.0, net.1 + 1), except, out);
        walk((net.0 | half, net.1 + 1), except, out);
    }
    walk(net, &except, &mut out);
    out
}

/// What of a pod matters to other pods' rules (as a peer) and its own (as
/// a target): a change elsewhere is not worth a recomputation.
pub fn fingerprint(pod: &Value) -> Value {
    json!({
        "labels": pod.pointer("/metadata/labels"),
        "ip": pod.pointer("/status/podIP"),
        "terminal": util::is_terminal(pod),
        "hostNetwork": host_network(pod),
        "ports": pod.pointer("/spec/containers").and_then(Value::as_array).map(|cs| cs.iter().map(|c| c.get("ports").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(name: &str, labels: Value) -> Value {
        json!({"metadata": {"name": name, "labels": labels}})
    }

    fn pod(ns: &str, name: &str, labels: Value, ip: &str) -> Value {
        json!({
            "metadata": {"namespace": ns, "name": name, "uid": format!("uid-{name}"), "labels": labels},
            "spec": {"containers": [{"name": "c", "ports": [{"name": "http", "containerPort": 8080}, {"name": "dns", "containerPort": 5353, "protocol": "UDP"}]}]},
            "status": {"phase": "Running", "podIP": ip},
        })
    }

    fn policy(ns: &str, spec: Value) -> Value {
        json!({"metadata": {"namespace": ns, "name": "p"}, "spec": spec})
    }

    fn tcp(p: u16) -> PortRange {
        PortRange { protocol: "tcp".into(), port: Some((p, p)) }
    }

    fn world() -> (Vec<Value>, Vec<Value>) {
        let namespaces = vec![ns("shop", json!({"team": "a"})), ns("other", json!({"team": "b"})), ns("monitoring", json!({"team": "a", "role": "mon"}))];
        let pods = vec![
            pod("shop", "db", json!({"app": "mongo"}), "10.42.0.5"),
            pod("shop", "api", json!({"app.kubernetes.io/name": "api", "app.kubernetes.io/part-of": "shop"}), "10.42.1.7"),
            pod("shop", "worker", json!({"app.kubernetes.io/name": "worker", "app.kubernetes.io/part-of": "shop"}), "10.42.0.9"),
            pod("other", "api", json!({"app.kubernetes.io/name": "api", "app.kubernetes.io/part-of": "shop"}), "10.42.2.3"),
            pod("monitoring", "prom", json!({"app": "prom"}), "10.42.2.4"),
        ];
        (namespaces, pods)
    }

    #[test]
    fn not_selected_is_not_isolated() {
        let (namespaces, pods) = world();
        let s = Snapshot::new(vec![policy("shop", json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "ingress": [{}]}))], namespaces, &pods);
        assert!(s.rules_for(&pods[1]).is_none());
        assert!(s.rules_for(&pods[3]).is_none());
        assert!(Snapshot::new(vec![], vec![], &pods).rules_for(&pods[0]).is_none());
    }

    #[test]
    fn gitops_allowed_clients_shape() {
        // The umbrella charts' datastore policy: same-namespace pods by name + part-of, one port.
        let (namespaces, pods) = world();
        let p = policy(
            "shop",
            json!({
                "podSelector": {"matchLabels": {"app": "mongo"}},
                "policyTypes": ["Ingress"],
                "ingress": [{"from": [
                    {"podSelector": {"matchLabels": {"app.kubernetes.io/name": "api", "app.kubernetes.io/part-of": "shop"}}},
                    {"podSelector": {"matchLabels": {"app.kubernetes.io/name": "worker", "app.kubernetes.io/part-of": "shop"}}},
                ], "ports": [{"port": 27017, "protocol": "TCP"}]}],
            }),
        );
        let s = Snapshot::new(vec![p], namespaces, &pods);
        let r = s.rules_for(&pods[0]).unwrap();
        assert!(r.egress.is_none());
        // Not the look-alike in namespace `other`.
        assert_eq!(r.ingress.unwrap(), vec![Allow { peers: Some(vec!["10.42.0.9/32".into(), "10.42.1.7/32".into()]), dns: false, ports: vec![tcp(27017)] }]);
    }

    #[test]
    fn default_deny_and_allow_all() {
        let (namespaces, pods) = world();
        let deny = policy("shop", json!({"podSelector": {}, "policyTypes": ["Ingress", "Egress"]}));
        let s = Snapshot::new(vec![deny.clone()], namespaces.clone(), &pods);
        assert_eq!(s.rules_for(&pods[1]).unwrap(), PodRules { ingress: Some(vec![]), egress: Some(vec![]) });
        // A second policy adds to the first: allow-all ingress.
        let allow = policy("shop", json!({"podSelector": {}, "ingress": [{}]}));
        let s = Snapshot::new(vec![deny, allow], namespaces, &pods);
        let r = s.rules_for(&pods[1]).unwrap();
        assert_eq!(r.ingress.unwrap(), vec![Allow { peers: None, dns: false, ports: vec![] }]);
        assert_eq!(r.egress.unwrap(), vec![]);
    }

    #[test]
    fn policy_types_default() {
        assert_eq!(policy_types(&policy("a", json!({"podSelector": {}}))), (true, false));
        assert_eq!(policy_types(&policy("a", json!({"podSelector": {}, "egress": []}))), (true, false));
        assert_eq!(policy_types(&policy("a", json!({"podSelector": {}, "egress": [{}]}))), (true, true));
        assert_eq!(policy_types(&policy("a", json!({"policyTypes": ["Egress"]}))), (false, true));
    }

    #[test]
    fn namespace_selector_alone_and_combined() {
        let (namespaces, pods) = world();
        let only_ns = policy("shop", json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "ingress": [{"from": [{"namespaceSelector": {"matchLabels": {"team": "a"}}}]}]}));
        let s = Snapshot::new(vec![only_ns], namespaces.clone(), &pods);
        // shop's pods (incl. the db itself) and monitoring's, not other's.
        assert_eq!(s.rules_for(&pods[0]).unwrap().ingress.unwrap()[0].peers.as_deref().unwrap(), ["10.42.0.5/32", "10.42.0.9/32", "10.42.1.7/32", "10.42.2.4/32"]);
        let both = policy(
            "shop",
            json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "ingress": [{"from": [{
                "namespaceSelector": {"matchExpressions": [{"key": "role", "operator": "In", "values": ["mon"]}]},
                "podSelector": {"matchLabels": {"app": "prom"}},
            }]}]}),
        );
        let s = Snapshot::new(vec![both], namespaces.clone(), &pods);
        assert_eq!(s.rules_for(&pods[0]).unwrap().ingress.unwrap()[0].peers.as_deref().unwrap(), ["10.42.2.4/32"]);
        // By the automatic name label.
        let by_name = policy("shop", json!({"podSelector": {}, "ingress": [{"from": [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "other"}}}]}]}));
        let s = Snapshot::new(vec![by_name], namespaces, &pods);
        assert_eq!(s.rules_for(&pods[0]).unwrap().ingress.unwrap()[0].peers.as_deref().unwrap(), ["10.42.2.3/32"]);
    }

    #[test]
    fn selectors_matching_nothing_allow_nothing() {
        let (namespaces, pods) = world();
        let p = policy("shop", json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "ingress": [{"from": [{"podSelector": {"matchLabels": {"app": "ghost"}}}]}]}));
        let s = Snapshot::new(vec![p], namespaces, &pods);
        assert_eq!(s.rules_for(&pods[0]).unwrap(), PodRules { ingress: Some(vec![]), egress: None });
    }

    #[test]
    fn ip_block_with_except() {
        assert_eq!(subtract("10.0.0.0/8", &[]), vec!["10.0.0.0/8"]);
        assert_eq!(subtract("10.0.0.0/30", &["10.0.0.1/32"]), vec!["10.0.0.0/32", "10.0.0.2/31"]);
        assert_eq!(subtract("10.0.0.0/24", &["10.0.0.0/25"]), vec!["10.0.0.128/25"]);
        assert_eq!(subtract("10.0.0.0/24", &["10.0.0.0/16"]), Vec::<String>::new());
        // 7 siblings of 10/8 outside 128/1, and 128/1 cut down around 192.168/16 (15).
        assert_eq!(subtract("0.0.0.0/0", &["10.0.0.0/8", "192.168.0.0/16"]).len(), 7 + 15);
        assert!(subtract("fd00::/8", &[]).is_empty());
        assert_eq!(subtract("10.1.2.3/24", &[]), vec!["10.1.2.0/24"]);
        let (namespaces, pods) = world();
        let p = policy(
            "shop",
            json!({"podSelector": {}, "policyTypes": ["Egress"], "egress": [{"to": [{"ipBlock": {"cidr": "10.42.0.0/16", "except": ["10.42.0.0/24"]}}], "ports": [{"port": 443}]}]}),
        );
        let s = Snapshot::new(vec![p], namespaces, &pods);
        let r = s.rules_for(&pods[1]).unwrap();
        assert!(r.ingress.is_none());
        let e = r.egress.unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].ports, vec![tcp(443)]);
        let mut want = vec!["10.42.1.0/24", "10.42.2.0/23", "10.42.4.0/22", "10.42.8.0/21", "10.42.16.0/20", "10.42.32.0/19", "10.42.64.0/18", "10.42.128.0/17"];
        want.sort();
        assert_eq!(e[0].peers.as_deref().unwrap(), want);
    }

    #[test]
    fn ports_ranges_protocols_and_named() {
        let (namespaces, pods) = world();
        let p = policy(
            "shop",
            json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "ingress": [{"ports": [
                {"port": 8000, "endPort": 8100},
                {"protocol": "UDP", "port": 53},
                {"protocol": "SCTP"},
                {"port": "http"},
                {"port": "dns", "protocol": "UDP"},
                {"port": "dns"},
                {"port": "nope"},
            ]}]}),
        );
        let s = Snapshot::new(vec![p], namespaces.clone(), &pods);
        let r = s.rules_for(&pods[0]).unwrap().ingress.unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].peers.is_none());
        assert_eq!(
            r[0].ports,
            vec![
                PortRange { protocol: "tcp".into(), port: Some((8000, 8100)) },
                PortRange { protocol: "udp".into(), port: Some((53, 53)) },
                PortRange { protocol: "sctp".into(), port: None },
                tcp(8080),
                PortRange { protocol: "udp".into(), port: Some((5353, 5353)) },
            ]
        );
        // Only named ports the pod lacks: the rule allows nothing.
        let p = policy("shop", json!({"podSelector": {}, "ingress": [{"ports": [{"port": "nope"}]}]}));
        let s = Snapshot::new(vec![p], namespaces, &pods);
        assert_eq!(s.rules_for(&pods[0]).unwrap().ingress.unwrap(), vec![]);
    }

    #[test]
    fn egress_named_ports_resolve_per_peer() {
        let (namespaces, mut pods) = world();
        pods[2]["spec"]["containers"][0]["ports"][0]["containerPort"] = json!(9090);
        let p = policy(
            "shop",
            json!({"podSelector": {"matchLabels": {"app": "mongo"}}, "policyTypes": ["Egress"], "egress": [{
                "to": [{"podSelector": {"matchLabels": {"app.kubernetes.io/part-of": "shop"}}}],
                "ports": [{"port": "http"}, {"port": 27017}],
            }]}),
        );
        let s = Snapshot::new(vec![p], namespaces, &pods);
        let e = s.rules_for(&pods[0]).unwrap().egress.unwrap();
        assert_eq!(
            e,
            vec![
                Allow { peers: Some(vec!["10.42.0.9/32".into()]), dns: false, ports: vec![tcp(9090)] },
                Allow { peers: Some(vec!["10.42.0.9/32".into(), "10.42.1.7/32".into()]), dns: false, ports: vec![tcp(27017)] },
                Allow { peers: Some(vec!["10.42.1.7/32".into()]), dns: false, ports: vec![tcp(8080)] },
            ]
        );
    }

    #[test]
    fn dns_only_when_allowed() {
        let (namespaces, pods) = world();
        // The usual "allow DNS" rule selects the virtual CoreDNS pod.
        let p = policy(
            "shop",
            json!({"podSelector": {}, "policyTypes": ["Egress"], "egress": [{
                "to": [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}}, "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}}}],
                "ports": [{"port": 53, "protocol": "UDP"}, {"port": 53, "protocol": "TCP"}],
            }]}),
        );
        let s = Snapshot::new(vec![p], namespaces.clone(), &pods);
        let e = s.rules_for(&pods[0]).unwrap().egress.unwrap();
        assert_eq!(e, vec![Allow { peers: Some(vec![]), dns: true, ports: vec![PortRange { protocol: "udp".into(), port: Some((53, 53)) }, tcp(53)] }]);
        // Named: dns / dns-tcp are CoreDNS's port names.
        let p = policy("shop", json!({"podSelector": {}, "policyTypes": ["Egress"], "egress": [{"to": [{"namespaceSelector": {}}], "ports": [{"port": "dns-tcp"}]}]}));
        let s = Snapshot::new(vec![p], namespaces.clone(), &pods);
        assert_eq!(s.rules_for(&pods[0]).unwrap().egress.unwrap(), vec![Allow { peers: Some(vec![]), dns: true, ports: vec![tcp(53)] }]);
        // Egress to the namespace's pods only: no DNS.
        let p = policy("shop", json!({"podSelector": {}, "policyTypes": ["Egress"], "egress": [{"to": [{"podSelector": {}}]}]}));
        let s = Snapshot::new(vec![p], namespaces, &pods);
        let e = s.rules_for(&pods[0]).unwrap().egress.unwrap();
        assert_eq!(e.len(), 1);
        assert!(!e[0].dns);
    }

    #[test]
    fn peers_need_an_address_and_host_network_is_left_alone() {
        let (namespaces, mut pods) = world();
        pods[1]["status"]["podIP"] = json!(null);
        pods[2]["status"]["phase"] = json!("Succeeded");
        pods[0]["spec"]["hostNetwork"] = json!(true);
        let p = policy("shop", json!({"podSelector": {}, "ingress": [{"from": [{"podSelector": {}}]}]}));
        let s = Snapshot::new(vec![p], namespaces, &pods);
        assert!(s.rules_for(&pods[0]).is_none());
        assert_eq!(s.rules_for(&pods[1]).unwrap().ingress.unwrap(), vec![]);
    }

    #[test]
    fn fingerprint_ignores_status_noise() {
        let (_, pods) = world();
        let mut changed = pods[0].clone();
        changed["status"]["conditions"] = json!([{"type": "Ready", "status": "True"}]);
        assert_eq!(fingerprint(&pods[0]), fingerprint(&changed));
        changed["metadata"]["labels"]["x"] = json!("y");
        assert_ne!(fingerprint(&pods[0]), fingerprint(&changed));
    }
}
