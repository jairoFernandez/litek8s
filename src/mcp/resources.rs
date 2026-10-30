//! What `litek8s mcp` serves as resources and `litek8s skill` prints: the
//! agent skill (skills/litek8s) and the docs, compiled into the binary so
//! they match the version that runs.

use serde_json::{Value, json};

/// A file of the skill: its path under skills/litek8s/, what it covers.
pub struct Doc {
    pub path: &'static str,
    pub description: &'static str,
    pub text: &'static str,
}

pub const SKILL: &[Doc] = &[
    Doc { path: "SKILL.md", description: "The litek8s agent skill: when to use litek8s vs kubectl, the tools, workflows, guardrails", text: include_str!("../../skills/litek8s/SKILL.md") },
    Doc { path: "references/dev-clusters.md", description: "litek8s dev: cluster lifecycle, names, nodes, contexts, images, settings", text: include_str!("../../skills/litek8s/references/dev-clusters.md") },
    Doc { path: "references/observability.md", description: "Built-in metrics, log search and alerts, with ready PromQL recipes", text: include_str!("../../skills/litek8s/references/observability.md") },
    Doc { path: "references/docker.md", description: "The Docker Engine API of a dev cluster: docker, compose, builds, what is missing", text: include_str!("../../skills/litek8s/references/docker.md") },
    Doc { path: "references/ha-and-join.md", description: "HA control plane (3/5 servers over Raft), joining nodes, the load balancer", text: include_str!("../../skills/litek8s/references/ha-and-join.md") },
    Doc { path: "references/auth-and-rbac.md", description: "Authentication and RBAC on servers: 401/403 after upgrading, RBAC-audit, fixes", text: include_str!("../../skills/litek8s/references/auth-and-rbac.md") },
    Doc { path: "references/differences.md", description: "How litek8s differs from upstream Kubernetes (conformance gaps, missing pieces)", text: include_str!("../../skills/litek8s/references/differences.md") },
    Doc { path: "references/troubleshooting.md", description: "Diagnosing pods, nodes, dev clusters and the API server", text: include_str!("../../skills/litek8s/references/troubleshooting.md") },
];

pub const DOCS: &[Doc] = &[
    Doc { path: "agents.md", description: "Using litek8s from LLM agents: the MCP server, the skill, -o json", text: include_str!("../../docs/agents.md") },
    Doc { path: "auth.md", description: "Authentication and RBAC: modes, identities, upgrading a cluster made with an earlier litek8s, your own users", text: include_str!("../../docs/auth.md") },
    Doc { path: "clusters.md", description: "Servers and nodes over SSH, cluster files (litek8s cluster apply)", text: include_str!("../../docs/clusters.md") },
    Doc { path: "limitations.md", description: "What litek8s does not do yet", text: include_str!("../../docs/limitations.md") },
    Doc { path: "dev.md", description: "litek8s dev: local clusters (design, commands, internals)", text: include_str!("../../docs/dev.md") },
    Doc { path: "docker.md", description: "The Docker Engine API on litek8s", text: include_str!("../../docs/docker.md") },
    Doc { path: "observability.md", description: "Built-in metrics, PromQL subset, log search, alerts", text: include_str!("../../docs/observability.md") },
    Doc { path: "ha.md", description: "HA control plane with Raft", text: include_str!("../../docs/ha.md") },
    Doc { path: "networking.md", description: "The pod overlay across nodes: NAT, relays, routes, overlay metrics", text: include_str!("../../docs/networking.md") },
    Doc { path: "registries.md", description: "Private registries and mirrors for image pulls (registries.yaml)", text: include_str!("../../docs/registries.md") },
    Doc { path: "crds.md", description: "CRDs: schemas, CEL, server-side apply", text: include_str!("../../docs/crds.md") },
    Doc { path: "ui.md", description: "litek8s ui, the terminal UI", text: include_str!("../../docs/ui.md") },
    Doc { path: "litecd.md", description: "GitOps with litecd: Argo CD manifests in one small pod; install, move over from Argo CD, numbers", text: include_str!("../../docs/litecd.md") },
    Doc { path: "CONFORMANCE.md", description: "Kubernetes conformance score and what fails", text: include_str!("../../compat/CONFORMANCE.md") },
];

pub fn list() -> Vec<Value> {
    let skill = SKILL.iter().map(|d| json!({"uri": format!("litek8s://skill/{}", d.path), "name": format!("skill/{}", d.path), "description": d.description, "mimeType": "text/markdown", "size": d.text.len()}));
    let docs = DOCS.iter().map(|d| json!({"uri": format!("litek8s://docs/{}", d.path), "name": format!("docs/{}", d.path), "description": d.description, "mimeType": "text/markdown", "size": d.text.len()}));
    skill.chain(docs).collect()
}

/// A resource's text by URI (only the embedded ones: no file access).
pub fn read(uri: &str) -> Option<&'static str> {
    if let Some(p) = uri.strip_prefix("litek8s://skill/") {
        return SKILL.iter().find(|d| d.path == p).map(|d| d.text);
    }
    let p = uri.strip_prefix("litek8s://docs/")?;
    DOCS.iter().find(|d| d.path == p).map(|d| d.text)
}

/// A skill file by short name: `SKILL.md`, `observability`,
/// `references/observability.md`.
pub fn skill_file(name: &str) -> Option<&'static Doc> {
    let n = name.trim_end_matches(".md");
    SKILL.iter().find(|d| d.path == name || d.path.trim_end_matches(".md") == n || d.path.trim_end_matches(".md").rsplit('/').next() == Some(n))
}
