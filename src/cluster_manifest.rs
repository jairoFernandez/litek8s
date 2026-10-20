//! Declarative cluster manifest (`kind: Cluster`).
//!
//! Single YAML file that captures the cross-cloud topology (VPN, nodes,
//! workloads) so demos are reproducible. Applied by orchestration logic that
//! composes existing `vpn`, `node add`, and `apply` calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const API_VERSION: &str = "litek8s.dev/v1alpha1";
pub const KIND: &str = "Cluster";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: ClusterMetadata,
    pub spec: ClusterSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterMetadata {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSpec {
    #[serde(default)]
    pub vpn: Option<ClusterVpnSpec>,
    #[serde(default)]
    pub nodes: Vec<ClusterNodeSpec>,
    #[serde(default)]
    pub workloads: Vec<ClusterWorkloadSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterVpnSpec {
    pub cidr: String,
    pub hub_endpoint: String,
    /// "this" means the daemon applying the manifest IS the hub.
    /// Anything else means the hub is remote (not yet implemented; reserved).
    #[serde(default = "default_hub_role")]
    pub hub_role: String,
}

fn default_hub_role() -> String {
    "this".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterNodeSpec {
    pub name: String,
    pub ssh: String,
    #[serde(default)]
    pub vpn_ip: Option<String>,
    #[serde(default)]
    pub install: bool,
    #[serde(default = "default_remote_binary")]
    pub remote_binary: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_remote_binary() -> String {
    crate::node::DEFAULT_REMOTE_BINARY.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterWorkloadSpec {
    pub file: PathBuf,
}

impl ClusterManifest {
    pub fn parse(raw: &str) -> Result<Self> {
        let manifest: ClusterManifest =
            serde_yaml::from_str(raw).context("failed to parse cluster manifest as YAML")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn parse_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == API_VERSION,
            "unsupported apiVersion `{}`, expected `{API_VERSION}`",
            self.api_version
        );
        ensure!(
            self.kind == KIND,
            "unexpected kind `{}`, expected `{KIND}`",
            self.kind
        );
        ensure!(
            !self.metadata.name.trim().is_empty(),
            "metadata.name must not be empty"
        );

        if let Some(vpn) = self.spec.vpn.as_ref() {
            let (_, prefix) = parse_cidr(&vpn.cidr)?;
            ensure!(
                prefix <= 30,
                "spec.vpn.cidr `{}` must use a /30 or shorter prefix",
                vpn.cidr
            );
            ensure!(
                !vpn.hub_endpoint.trim().is_empty(),
                "spec.vpn.hubEndpoint must not be empty"
            );
            ensure!(
                vpn.hub_role == "this",
                "spec.vpn.hubRole `{}` not supported yet; use `this`",
                vpn.hub_role
            );
        }

        let mut seen = std::collections::HashSet::new();
        for node in &self.spec.nodes {
            ensure!(
                !node.name.trim().is_empty(),
                "spec.nodes[].name must not be empty"
            );
            ensure!(
                !seen.contains(&node.name),
                "duplicate node name `{}`",
                node.name
            );
            seen.insert(node.name.clone());
            ensure!(
                !node.ssh.trim().is_empty(),
                "spec.nodes[].ssh must not be empty (`{}`)",
                node.name
            );
            crate::node::validate_node_name(&node.name)?;
            crate::node::validate_labels(&node.labels)?;
        }

        for workload in &self.spec.workloads {
            ensure!(
                workload.file.as_os_str().len() > 0,
                "spec.workloads[].file must not be empty"
            );
        }

        Ok(())
    }
}

/// Parse an IPv4 CIDR into `(base, prefix_len)`.
pub fn parse_cidr(value: &str) -> Result<(std::net::Ipv4Addr, u8)> {
    let (base, prefix) = value
        .split_once('/')
        .with_context(|| format!("CIDR `{value}` must be in `A.B.C.D/N` form"))?;
    let base: std::net::Ipv4Addr = base
        .parse()
        .with_context(|| format!("CIDR `{value}` has invalid IPv4 base"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("CIDR `{value}` has invalid prefix length"))?;
    if prefix > 32 {
        bail!("CIDR `{value}` prefix must be ≤ 32");
    }
    Ok((base, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
apiVersion: litek8s.dev/v1alpha1
kind: Cluster
metadata:
  name: demo-cross-cloud
spec:
  vpn:
    cidr: 10.42.0.0/24
    hubEndpoint: 54.1.2.3:51820
  nodes:
    - name: civo-1
      ssh: ubuntu@159.223.1.2
      vpnIp: 10.42.0.2
      install: true
      labels:
        role: edge
    - name: laptop
      ssh: me@home
      vpnIp: 10.42.0.3
  workloads:
    - file: examples/nginx.yaml
"#;

    #[test]
    fn parses_sample_manifest() {
        let manifest = ClusterManifest::parse(SAMPLE).expect("parse");
        assert_eq!(manifest.metadata.name, "demo-cross-cloud");
        assert_eq!(manifest.spec.nodes.len(), 2);
        assert_eq!(manifest.spec.workloads.len(), 1);
        let vpn = manifest.spec.vpn.expect("vpn");
        assert_eq!(vpn.cidr, "10.42.0.0/24");
        assert_eq!(vpn.hub_role, "this");
    }

    #[test]
    fn rejects_unsupported_api_version() {
        let bad = SAMPLE.replace("litek8s.dev/v1alpha1", "v2");
        assert!(ClusterManifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_duplicate_node_names() {
        let bad = SAMPLE.replace("laptop", "civo-1");
        assert!(ClusterManifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_empty_ssh() {
        let bad = SAMPLE.replace("ssh: ubuntu@159.223.1.2", "ssh: \"\"");
        assert!(ClusterManifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_bad_cidr() {
        let bad = SAMPLE.replace("10.42.0.0/24", "not-a-cidr");
        assert!(ClusterManifest::parse(&bad).is_err());
    }

    #[test]
    fn parses_without_vpn_or_workloads() {
        let trimmed = r#"
apiVersion: litek8s.dev/v1alpha1
kind: Cluster
metadata:
  name: minimal
spec:
  nodes:
    - name: edge-1
      ssh: ubuntu@10.0.0.5
"#;
        let manifest = ClusterManifest::parse(trimmed).expect("parse");
        assert!(manifest.spec.vpn.is_none());
        assert!(manifest.spec.workloads.is_empty());
    }
}
