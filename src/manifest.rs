use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const API_VERSION: &str = "litek8s.dev/v1alpha1";
pub const KIND_DEPLOYMENT: &str = "Deployment";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: DeploymentSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentSpec {
    pub replicas: Option<u32>,
    pub template: PodTemplateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodTemplateSpec {
    pub spec: PodSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodSpec {
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub containers: Vec<ContainerSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub ports: Vec<ContainerPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerPort {
    pub container_port: u16,
    pub host_port: Option<u16>,
    #[serde(default)]
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
}

impl DeploymentManifest {
    pub fn parse(input: &str) -> Result<Self> {
        let manifest: Self =
            serde_yaml::from_str(input).context("failed to parse deployment manifest YAML")?;
        manifest.validate()
    }

    pub fn validate(mut self) -> Result<Self> {
        ensure!(
            self.api_version == API_VERSION,
            "unsupported apiVersion `{}`; expected `{API_VERSION}`",
            self.api_version
        );
        ensure!(
            self.kind == KIND_DEPLOYMENT,
            "unsupported kind `{}`; expected `{KIND_DEPLOYMENT}`",
            self.kind
        );
        ensure!(
            !self.metadata.name.trim().is_empty(),
            "metadata.name must not be empty"
        );

        let replicas = self.spec.replicas.unwrap_or(1);
        ensure!(replicas == 1, "spec.replicas must be set to 1 in v1");
        self.spec.replicas = Some(1);

        let pod_spec = &mut self.spec.template.spec;
        let restart_policy = pod_spec
            .restart_policy
            .clone()
            .unwrap_or(RestartPolicy::Always);
        ensure!(
            restart_policy == RestartPolicy::Always,
            "spec.template.spec.restartPolicy must be `Always`"
        );
        pod_spec.restart_policy = Some(RestartPolicy::Always);

        ensure!(
            pod_spec.containers.len() == 1,
            "spec.template.spec.containers must contain exactly one container"
        );

        let container = &pod_spec.containers[0];
        ensure!(
            !container.name.trim().is_empty(),
            "container.name must not be empty"
        );
        ensure!(
            !container.image.trim().is_empty(),
            "container.image must not be empty"
        );

        for env_var in &container.env {
            ensure!(
                !env_var.name.trim().is_empty(),
                "env.name must not be empty"
            );
        }

        for port in &container.ports {
            ensure!(
                port.container_port > 0,
                "containerPort must be greater than zero"
            );
            if let Some(host_port) = port.host_port {
                ensure!(host_port > 0, "hostPort must be greater than zero");
            }
        }

        Ok(self)
    }

    pub fn deployment_name(&self) -> &str {
        &self.metadata.name
    }

    pub fn container(&self) -> &ContainerSpec {
        &self.spec.template.spec.containers[0]
    }

    pub fn desired_hash(&self) -> Result<String> {
        let canonical = serde_json::to_vec(self).context("failed to canonicalize manifest")?;
        let mut hasher = Sha256::new();
        hasher.update(canonical);
        Ok(hex::encode(hasher.finalize()))
    }
}

impl ContainerPort {
    pub fn docker_port_key(&self) -> String {
        format!(
            "{}/{}",
            self.container_port,
            self.protocol.as_docker_suffix()
        )
    }

    pub fn printable(&self) -> String {
        match self.host_port {
            Some(host_port) => format!(
                "{}->{}/{}",
                host_port,
                self.container_port,
                self.protocol.as_printable()
            ),
            None => format!("{}/{}", self.container_port, self.protocol.as_printable()),
        }
    }
}

impl PortProtocol {
    pub fn as_docker_suffix(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    pub fn as_printable(&self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{API_VERSION, DeploymentManifest};

    fn manifest_yaml() -> String {
        format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
          command: ["nginx"]
          args: ["-g", "daemon off;"]
          env:
            - name: APP_ENV
              value: demo
          ports:
            - containerPort: 80
              hostPort: 8080
"#
        )
    }

    #[test]
    fn parses_and_validates_supported_manifest() {
        let manifest = DeploymentManifest::parse(&manifest_yaml()).expect("manifest should parse");
        assert_eq!(manifest.deployment_name(), "demo");
        assert_eq!(manifest.container().image, "nginx:alpine");
    }

    #[test]
    fn rejects_multiple_containers() {
        let yaml = format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
        - name: sidecar
          image: alpine:latest
"#
        );

        let error = DeploymentManifest::parse(&yaml).expect_err("manifest should be rejected");
        assert!(
            error
                .to_string()
                .contains("must contain exactly one container"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_non_default_replicas() {
        let yaml = format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: 2
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
"#
        );

        let error = DeploymentManifest::parse(&yaml).expect_err("manifest should be rejected");
        assert!(error.to_string().contains("spec.replicas"));
    }
}
