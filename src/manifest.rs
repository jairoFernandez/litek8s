use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::node::{validate_label_key, validate_label_value, validate_node_name};

pub const API_VERSION: &str = "litek8s.dev/v1alpha1";
pub const KIND_DEPLOYMENT: &str = "Deployment";
pub const KIND_SERVICE: &str = "Service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedManifest {
    Deployment(DeploymentManifest),
    Service(ServiceManifest),
}

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
pub struct ServiceManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ServiceSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentSpec {
    pub replicas: Option<u32>,
    pub template: PodTemplateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceSpec {
    pub deployment_name: Option<String>,
    pub listen_addr: Option<String>,
    pub target_port: Option<u16>,
    #[serde(default)]
    pub selector: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<ServicePort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServicePort {
    pub port: u16,
    pub target_port: Option<u16>,
    #[serde(default)]
    pub protocol: PortProtocol,
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
    pub node_name: Option<String>,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
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
    pub resources: Option<ResourceRequirements>,
    pub readiness_probe: Option<ReadinessProbe>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessProbe {
    pub tcp_socket: TcpSocketProbe,
    pub initial_delay_seconds: Option<u64>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TcpSocketProbe {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRequirements {
    pub limits: Option<ResourceLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLimits {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedResourceLimits {
    pub nano_cpus: Option<i64>,
    pub memory_bytes: Option<i64>,
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
        for (key, value) in &self.metadata.labels {
            validate_label_key(key)?;
            validate_label_value(value)?;
        }

        let replicas = self.spec.replicas.unwrap_or(1);
        ensure!(replicas >= 1, "spec.replicas must be at least 1");
        self.spec.replicas = Some(replicas);

        let pod_spec = &mut self.spec.template.spec;
        if let Some(node_name) = pod_spec.node_name.as_deref() {
            validate_node_name(node_name)?;
        }
        for (key, value) in &pod_spec.node_selector {
            validate_label_key(key)?;
            validate_label_value(value)?;
        }
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

        let _ = container.normalized_resource_limits()?;
        if let Some(probe) = container.readiness_probe.as_ref() {
            ensure!(
                probe.tcp_socket.port > 0,
                "readinessProbe.tcpSocket.port must be greater than zero"
            );
            if let Some(timeout_seconds) = probe.timeout_seconds {
                ensure!(
                    timeout_seconds > 0,
                    "readinessProbe.timeoutSeconds must be greater than zero"
                );
            }
        }

        Ok(self)
    }

    pub fn deployment_name(&self) -> &str {
        &self.metadata.name
    }

    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.metadata.labels
    }

    pub fn container(&self) -> &ContainerSpec {
        &self.spec.template.spec.containers[0]
    }

    pub fn explicit_node_name(&self) -> Option<&str> {
        self.spec.template.spec.node_name.as_deref()
    }

    pub fn node_selector(&self) -> &BTreeMap<String, String> {
        &self.spec.template.spec.node_selector
    }

    pub fn matches_labels(&self, selector: &BTreeMap<String, String>) -> bool {
        selector
            .iter()
            .all(|(key, value)| self.metadata.labels.get(key) == Some(value))
    }

    pub fn desired_hash(&self) -> Result<String> {
        let canonical = serde_json::to_vec(self).context("failed to canonicalize manifest")?;
        let mut hasher = Sha256::new();
        hasher.update(canonical);
        Ok(hex::encode(hasher.finalize()))
    }

    /// Desired replica count for this deployment (defaults to 1).
    pub fn desired_replicas(&self) -> u32 {
        self.spec.replicas.unwrap_or(1)
    }

    /// Hash of the pod template ONLY (not the replica count). This is the
    /// per-container identity used to decide whether a running container
    /// matches the desired spec. Scaling the replica count up or down must NOT
    /// change this hash, otherwise every existing container would be seen as
    /// stale and torn down on the next reconcile (a recreation storm).
    pub fn pod_template_hash(&self) -> Result<String> {
        let canonical = serde_json::to_vec(&self.spec.template)
            .context("failed to canonicalize pod template")?;
        let mut hasher = Sha256::new();
        hasher.update(canonical);
        Ok(hex::encode(hasher.finalize()))
    }

    pub fn proxy_port(&self, requested: Option<u16>) -> Result<&ContainerPort> {
        let published_ports: Vec<&ContainerPort> = self
            .container()
            .ports
            .iter()
            .filter(|port| port.host_port.is_some())
            .collect();

        ensure!(
            !published_ports.is_empty(),
            "deployment `{}` does not publish any hostPort",
            self.deployment_name()
        );

        if let Some(requested) = requested {
            return published_ports
                .into_iter()
                .find(|port| {
                    port.protocol == PortProtocol::Tcp
                        && (port.container_port == requested || port.host_port == Some(requested))
                })
                .with_context(|| {
                    format!(
                        "deployment `{}` does not publish TCP port `{requested}`",
                        self.deployment_name()
                    )
                });
        }

        let tcp_ports: Vec<&ContainerPort> = published_ports
            .into_iter()
            .filter(|port| port.protocol == PortProtocol::Tcp)
            .collect();
        ensure!(
            !tcp_ports.is_empty(),
            "deployment `{}` does not publish a TCP hostPort",
            self.deployment_name()
        );
        ensure!(
            tcp_ports.len() == 1,
            "deployment `{}` publishes multiple TCP ports; pass --port",
            self.deployment_name()
        );
        Ok(tcp_ports[0])
    }
}

impl ServiceManifest {
    pub fn parse(input: &str) -> Result<Self> {
        let manifest: Self =
            serde_yaml::from_str(input).context("failed to parse service manifest YAML")?;
        manifest.validate()
    }

    pub fn validate(self) -> Result<Self> {
        ensure!(
            self.api_version == API_VERSION,
            "unsupported apiVersion `{}`; expected `{API_VERSION}`",
            self.api_version
        );
        ensure!(
            self.kind == KIND_SERVICE,
            "unsupported kind `{}`; expected `{KIND_SERVICE}`",
            self.kind
        );
        ensure!(
            !self.metadata.name.trim().is_empty(),
            "metadata.name must not be empty"
        );
        let has_deployment_name = self
            .spec
            .deployment_name
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let has_selector = !self.spec.selector.is_empty();
        ensure!(
            has_deployment_name ^ has_selector,
            "spec must set exactly one of deploymentName or selector"
        );
        if let Some(deployment_name) = self.spec.deployment_name.as_deref() {
            ensure!(
                !deployment_name.trim().is_empty(),
                "spec.deploymentName must not be empty"
            );
        }
        for (key, value) in &self.spec.selector {
            validate_label_key(key)?;
            validate_label_value(value)?;
        }

        let has_listen_addr = self
            .spec
            .listen_addr
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let has_ports = !self.spec.ports.is_empty();
        ensure!(
            has_listen_addr ^ has_ports,
            "spec must set exactly one of listenAddr or ports"
        );
        if let Some(listen_addr) = self.spec.listen_addr.as_deref() {
            listen_addr.parse::<SocketAddr>().with_context(|| {
                format!("spec.listenAddr `{listen_addr}` must use IP:PORT syntax")
            })?;
        }
        if has_ports {
            ensure!(
                self.spec.ports.len() == 1,
                "spec.ports must contain exactly one port in v1"
            );
            let port = &self.spec.ports[0];
            ensure!(
                port.port > 0,
                "spec.ports[0].port must be greater than zero"
            );
            ensure!(
                port.protocol == PortProtocol::Tcp,
                "spec.ports[0].protocol must be TCP in v1"
            );
            if let Some(target_port) = port.target_port {
                ensure!(
                    target_port > 0,
                    "spec.ports[0].targetPort must be greater than zero"
                );
            }
            ensure!(
                self.spec.target_port.is_none(),
                "spec.targetPort cannot be used together with spec.ports"
            );
        }
        if let Some(target_port) = self.spec.target_port {
            ensure!(target_port > 0, "spec.targetPort must be greater than zero");
        }
        Ok(self)
    }

    pub fn service_name(&self) -> &str {
        &self.metadata.name
    }

    pub fn configured_deployment_name(&self) -> Option<&str> {
        self.spec.deployment_name.as_deref()
    }

    pub fn selector(&self) -> &BTreeMap<String, String> {
        &self.spec.selector
    }

    pub fn listen_addr(&self) -> Result<String> {
        if let Some(listen_addr) = self.spec.listen_addr.as_deref() {
            return Ok(listen_addr.to_string());
        }
        let port = self
            .spec
            .ports
            .first()
            .context("service does not define a listen port")?;
        Ok(format!("127.0.0.1:{}", port.port))
    }

    pub fn resolved_target_port(&self) -> Option<u16> {
        if let Some(target_port) = self.spec.target_port {
            return Some(target_port);
        }
        self.spec
            .ports
            .first()
            .map(|port| port.target_port.unwrap_or(port.port))
    }
}

impl ParsedManifest {
    pub fn parse(input: &str) -> Result<Self> {
        let envelope: ManifestEnvelope =
            serde_yaml::from_str(input).context("failed to parse manifest YAML header")?;
        match envelope.kind.as_str() {
            KIND_DEPLOYMENT => Ok(Self::Deployment(DeploymentManifest::parse(input)?)),
            KIND_SERVICE => Ok(Self::Service(ServiceManifest::parse(input)?)),
            other => bail!("unsupported kind `{other}`; expected `Deployment` or `Service`"),
        }
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

impl ContainerSpec {
    pub fn normalized_resource_limits(&self) -> Result<Option<NormalizedResourceLimits>> {
        match &self.resources {
            Some(resources) => Ok(Some(resources.normalized_limits()?)),
            None => Ok(None),
        }
    }

    pub fn cpu_limit(&self) -> Option<&str> {
        self.resources.as_ref()?.limits.as_ref()?.cpu.as_deref()
    }

    pub fn memory_limit(&self) -> Option<&str> {
        self.resources.as_ref()?.limits.as_ref()?.memory.as_deref()
    }

    pub fn readiness_probe(&self) -> Option<&ReadinessProbe> {
        self.readiness_probe.as_ref()
    }
}

impl ReadinessProbe {
    pub fn initial_delay_seconds(&self) -> u64 {
        self.initial_delay_seconds.unwrap_or(0)
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds.unwrap_or(1)
    }
}

impl ResourceRequirements {
    pub fn normalized_limits(&self) -> Result<NormalizedResourceLimits> {
        let limits = self
            .limits
            .as_ref()
            .context("resources.limits must be provided when resources is set")?;

        let nano_cpus = match limits.cpu.as_deref() {
            Some(value) => Some(parse_cpu_quantity(value)?),
            None => None,
        };
        let memory_bytes = match limits.memory.as_deref() {
            Some(value) => Some(parse_memory_quantity(value)?),
            None => None,
        };

        ensure!(
            nano_cpus.is_some() || memory_bytes.is_some(),
            "resources.limits must include at least one of cpu or memory"
        );

        Ok(NormalizedResourceLimits {
            nano_cpus,
            memory_bytes,
        })
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestEnvelope {
    _api_version: Option<String>,
    kind: String,
}

fn parse_cpu_quantity(value: &str) -> Result<i64> {
    let value = value.trim();
    ensure!(!value.is_empty(), "resources.limits.cpu must not be empty");

    if let Some(millis) = value.strip_suffix('m') {
        let millis = parse_positive_i64(millis, "resources.limits.cpu")?;
        return millis
            .checked_mul(1_000_000)
            .context("resources.limits.cpu is too large");
    }

    let cores = value
        .parse::<f64>()
        .with_context(|| format!("resources.limits.cpu `{value}` is not a valid CPU quantity"))?;
    ensure!(
        cores.is_finite() && cores > 0.0,
        "resources.limits.cpu must be greater than zero"
    );

    let nano_cpus = (cores * 1_000_000_000f64).round();
    ensure!(
        nano_cpus.is_finite() && nano_cpus > 0.0 && nano_cpus <= i64::MAX as f64,
        "resources.limits.cpu is too large"
    );
    Ok(nano_cpus as i64)
}

fn parse_memory_quantity(value: &str) -> Result<i64> {
    let value = value.trim();
    ensure!(
        !value.is_empty(),
        "resources.limits.memory must not be empty"
    );

    for (suffix, multiplier) in [
        ("Ki", 1024i64),
        ("Mi", 1024i64.pow(2)),
        ("Gi", 1024i64.pow(3)),
        ("Ti", 1024i64.pow(4)),
    ] {
        if let Some(raw) = value.strip_suffix(suffix) {
            let base = parse_positive_i64(raw, "resources.limits.memory")?;
            return base
                .checked_mul(multiplier)
                .context("resources.limits.memory is too large");
        }
    }

    parse_positive_i64(value, "resources.limits.memory")
}

fn parse_positive_i64(value: &str, field: &str) -> Result<i64> {
    let parsed = value
        .trim()
        .parse::<i64>()
        .with_context(|| format!("{field} `{value}` is not a valid positive integer"))?;
    ensure!(parsed > 0, "{field} must be greater than zero");
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::{API_VERSION, DeploymentManifest, ParsedManifest, ServiceManifest};

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
          resources:
            limits:
              cpu: "500m"
              memory: "128Mi"
"#
        )
    }

    #[test]
    fn parses_and_validates_supported_manifest() {
        let manifest = DeploymentManifest::parse(&manifest_yaml()).expect("manifest should parse");
        assert_eq!(manifest.deployment_name(), "demo");
        assert_eq!(manifest.container().image, "nginx:alpine");
        let limits = manifest
            .container()
            .normalized_resource_limits()
            .expect("limits should parse")
            .expect("limits should exist");
        assert_eq!(limits.nano_cpus, Some(500_000_000));
        assert_eq!(limits.memory_bytes, Some(134_217_728));
    }

    #[test]
    fn parses_supported_readiness_probe() {
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
          readinessProbe:
            tcpSocket:
              port: 80
            initialDelaySeconds: 2
            timeoutSeconds: 3
"#
        );

        let manifest = DeploymentManifest::parse(&yaml).expect("manifest should parse");
        let probe = manifest
            .container()
            .readiness_probe()
            .expect("probe should exist");
        assert_eq!(probe.tcp_socket.port, 80);
        assert_eq!(probe.initial_delay_seconds(), 2);
        assert_eq!(probe.timeout_seconds(), 3);
    }

    #[test]
    fn rejects_invalid_readiness_probe() {
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
          readinessProbe:
            tcpSocket:
              port: 80
            timeoutSeconds: 0
"#
        );

        let error = DeploymentManifest::parse(&yaml).expect_err("manifest should be rejected");
        assert!(error.to_string().contains("readinessProbe.timeoutSeconds"));
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

    fn replicas_yaml(replicas: u32) -> String {
        format!(
            r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: demo
spec:
  replicas: {replicas}
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: web
          image: nginx:alpine
"#
        )
    }

    #[test]
    fn accepts_multiple_replicas() {
        let manifest = DeploymentManifest::parse(&replicas_yaml(3)).expect("replicas: 3 is valid");
        assert_eq!(manifest.desired_replicas(), 3);
    }

    #[test]
    fn rejects_zero_replicas() {
        let error = DeploymentManifest::parse(&replicas_yaml(0))
            .expect_err("replicas: 0 should be rejected");
        assert!(error.to_string().contains("spec.replicas"));
    }

    #[test]
    fn pod_template_hash_ignores_replica_count() {
        // The per-container identity hash must be invariant under scaling, so
        // changing the replica count never recreates existing containers.
        let one = DeploymentManifest::parse(&replicas_yaml(1)).unwrap();
        let five = DeploymentManifest::parse(&replicas_yaml(5)).unwrap();
        assert_eq!(
            one.pod_template_hash().unwrap(),
            five.pod_template_hash().unwrap()
        );
        // ...while the whole-manifest hash does differ.
        assert_ne!(one.desired_hash().unwrap(), five.desired_hash().unwrap());
    }

    #[test]
    fn accepts_decimal_cpu_and_gib_memory_limits() {
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
          resources:
            limits:
              cpu: "0.5"
              memory: "1Gi"
"#
        );

        let manifest = DeploymentManifest::parse(&yaml).expect("manifest should parse");
        let limits = manifest
            .container()
            .normalized_resource_limits()
            .expect("limits should normalize")
            .expect("limits should exist");
        assert_eq!(limits.nano_cpus, Some(500_000_000));
        assert_eq!(limits.memory_bytes, Some(1_073_741_824));
    }

    #[test]
    fn rejects_requests_in_resources() {
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
          resources:
            requests:
              cpu: "250m"
"#
        );

        let error = DeploymentManifest::parse(&yaml).expect_err("manifest should be rejected");
        let text = format!("{error:#}");
        assert!(
            text.contains("requests") || text.contains("unknown field"),
            "unexpected error: {text}"
        );
    }

    #[test]
    fn rejects_zero_or_invalid_resource_limits() {
        let invalid_cpu = format!(
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
          resources:
            limits:
              cpu: "0"
"#
        );
        let invalid_memory = format!(
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
          resources:
            limits:
              memory: "128M"
"#
        );

        let cpu_error =
            DeploymentManifest::parse(&invalid_cpu).expect_err("cpu limit should be rejected");
        assert!(cpu_error.to_string().contains("greater than zero"));

        let memory_error = DeploymentManifest::parse(&invalid_memory)
            .expect_err("memory limit should be rejected");
        assert!(
            memory_error
                .to_string()
                .contains("not a valid positive integer")
        );
    }

    #[test]
    fn selects_single_tcp_proxy_port() {
        let manifest = DeploymentManifest::parse(&manifest_yaml()).expect("manifest should parse");
        let port = manifest
            .proxy_port(None)
            .expect("proxy port should resolve");
        assert_eq!(port.container_port, 80);
        assert_eq!(port.host_port, Some(8080));
    }

    #[test]
    fn rejects_ambiguous_proxy_ports_without_explicit_selection() {
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
        - name: echo
          image: hashicorp/http-echo:1.0.0
          ports:
            - containerPort: 5678
              hostPort: 8090
            - containerPort: 8080
              hostPort: 8091
"#
        );

        let manifest = DeploymentManifest::parse(&yaml).expect("manifest should parse");
        assert!(manifest.proxy_port(None).is_err());

        let by_container_port = manifest
            .proxy_port(Some(5678))
            .expect("containerPort match should resolve");
        assert_eq!(by_container_port.host_port, Some(8090));

        let by_host_port = manifest
            .proxy_port(Some(8091))
            .expect("hostPort match should resolve");
        assert_eq!(by_host_port.container_port, 8080);
    }

    #[test]
    fn parses_supported_service_manifest() {
        let manifest = ServiceManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Service
metadata:
  name: echo-svc
spec:
  selector:
    app: echo-demo
  ports:
    - port: 9100
      targetPort: 8090
"#
        ))
        .expect("service should parse");

        assert_eq!(manifest.service_name(), "echo-svc");
        assert_eq!(manifest.configured_deployment_name(), None);
        assert_eq!(
            manifest.selector().get("app").map(String::as_str),
            Some("echo-demo")
        );
        assert_eq!(
            manifest.listen_addr().expect("listen addr"),
            "127.0.0.1:9100"
        );
        assert_eq!(manifest.resolved_target_port(), Some(8090));
    }

    #[test]
    fn rejects_service_manifest_with_invalid_listen_addr() {
        let error = ServiceManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Service
metadata:
  name: echo-svc
spec:
  deploymentName: echo-demo
  listenAddr: localhost
"#
        ))
        .expect_err("service should be rejected");

        assert!(error.to_string().contains("spec.listenAddr"));
    }

    #[test]
    fn accepts_legacy_service_manifest_shape_for_compatibility() {
        let manifest = ServiceManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Service
metadata:
  name: echo-svc
spec:
  deploymentName: echo-demo
  listenAddr: 127.0.0.1:9100
  targetPort: 8090
"#
        ))
        .expect("legacy service should parse");

        assert_eq!(manifest.configured_deployment_name(), Some("echo-demo"));
        assert!(manifest.selector().is_empty());
        assert_eq!(
            manifest.listen_addr().expect("listen addr"),
            "127.0.0.1:9100"
        );
        assert_eq!(manifest.resolved_target_port(), Some(8090));
    }

    #[test]
    fn parses_generic_manifest_enum() {
        let deployment = ParsedManifest::parse(&manifest_yaml()).expect("deployment should parse");
        assert!(matches!(deployment, ParsedManifest::Deployment(_)));

        let service = ParsedManifest::parse(&format!(
            r#"
apiVersion: {API_VERSION}
kind: Service
metadata:
  name: echo-svc
spec:
  selector:
    app: echo-demo
  ports:
    - port: 9100
"#
        ))
        .expect("service should parse");
        assert!(matches!(service, ParsedManifest::Service(_)));
    }
}
