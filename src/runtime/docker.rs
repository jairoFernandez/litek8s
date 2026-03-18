use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bollard::API_DEFAULT_VERSION;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StartContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{HostConfig, PortBinding};
use bollard::system::EventsOptions;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::manifest::{ContainerPort, DeploymentManifest};

const LABEL_MANAGED: &str = "litek8s.managed";
const LABEL_DEPLOYMENT: &str = "litek8s.deployment";
const LABEL_HASH: &str = "litek8s.spec-hash";

#[derive(Clone)]
pub struct DockerRuntime {
    docker: Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedContainer {
    pub id: String,
    pub name: String,
    pub deployment: String,
    pub state: Option<String>,
    pub status: Option<String>,
    pub running: bool,
    pub spec_hash: Option<String>,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self> {
        let docker = connect_docker()?;
        Ok(Self { docker })
    }

    pub async fn ping(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .context("failed to ping the Docker daemon")?;
        Ok(())
    }

    pub fn spawn_event_listener(&self, notify: Arc<Notify>) -> tokio::task::JoinHandle<()> {
        let docker = self.docker.clone();
        tokio::spawn(async move {
            loop {
                let mut filters = HashMap::new();
                filters.insert("type".to_string(), vec!["container".to_string()]);
                filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);

                let mut events = docker.events(Some(EventsOptions::<String> {
                    since: None,
                    until: None,
                    filters,
                }));

                while let Some(next_event) = events.next().await {
                    match next_event {
                        Ok(_) => notify.notify_one(),
                        Err(error) => {
                            eprintln!("docker event stream ended: {error}");
                            break;
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    }

    pub async fn list_managed_containers(&self) -> Result<Vec<ManagedContainer>> {
        self.list_containers_with_filters(vec![format!("{LABEL_MANAGED}=true")])
            .await
    }

    pub async fn list_containers_for_deployment(
        &self,
        name: &str,
    ) -> Result<Vec<ManagedContainer>> {
        self.list_containers_with_filters(vec![
            format!("{LABEL_MANAGED}=true"),
            format!("{LABEL_DEPLOYMENT}={name}"),
        ])
        .await
    }

    pub async fn create_and_start(
        &self,
        manifest: &DeploymentManifest,
        spec_hash: &str,
    ) -> Result<String> {
        self.ensure_image(&manifest.container().image).await?;

        let container_name = deployment_container_name(manifest.deployment_name());
        let config = build_container_config(manifest, spec_hash);

        let create_result = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name,
                    platform: None,
                }),
                config,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to create container for deployment {}",
                    manifest.deployment_name()
                )
            })?;

        self.docker
            .start_container(&create_result.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| {
                format!(
                    "failed to start container for deployment {}",
                    manifest.deployment_name()
                )
            })?;

        Ok(create_result.id)
    }

    pub async fn remove_deployment(&self, name: &str) -> Result<()> {
        let containers = self.list_containers_for_deployment(name).await?;
        for container in containers {
            self.remove_container(&container.id).await?;
        }
        Ok(())
    }

    pub async fn remove_orphans(&self, desired_names: &HashSet<String>) -> Result<()> {
        for container in self.list_managed_containers().await? {
            if !desired_names.contains(&container.deployment) {
                self.remove_container(&container.id).await?;
            }
        }
        Ok(())
    }

    pub async fn remove_container(&self, container_id: &str) -> Result<()> {
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("failed to remove container {container_id}"))?;
        Ok(())
    }

    pub async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        let container = self
            .list_containers_for_deployment(name)
            .await?
            .into_iter()
            .next()
            .with_context(|| format!("deployment `{name}` has no managed container"))?;

        let mut stream = self.docker.logs(
            &container.id,
            Some(LogsOptions::<String> {
                follow: false,
                stdout: true,
                stderr: true,
                tail: tail.to_string(),
                ..Default::default()
            }),
        );

        let mut content = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.context("failed to fetch Docker logs")? {
                LogOutput::StdOut { message }
                | LogOutput::StdErr { message }
                | LogOutput::Console { message }
                | LogOutput::StdIn { message } => {
                    content.push_str(&String::from_utf8_lossy(&message));
                }
            }
        }

        Ok(content)
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }

        let mut pull_stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: image,
                ..Default::default()
            }),
            None,
            None,
        );

        while let Some(event) = pull_stream.next().await {
            event.with_context(|| format!("failed while pulling image `{image}`"))?;
        }

        Ok(())
    }

    async fn list_containers_with_filters(
        &self,
        labels: Vec<String>,
    ) -> Result<Vec<ManagedContainer>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), labels);

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .context("failed to list Docker containers")?;

        let mut items = Vec::with_capacity(containers.len());
        for container in containers {
            let labels = container.labels.unwrap_or_default();
            let deployment = labels
                .get(LABEL_DEPLOYMENT)
                .cloned()
                .ok_or_else(|| anyhow!("managed container missing deployment label"))?;
            let name = container
                .names
                .unwrap_or_default()
                .first()
                .map(|value| value.trim_start_matches('/').to_string())
                .unwrap_or_else(|| deployment_container_name(&deployment));

            items.push(ManagedContainer {
                id: container.id.unwrap_or_default(),
                name,
                deployment,
                state: container.state.clone(),
                status: container.status.clone(),
                running: container.state.as_deref() == Some("running"),
                spec_hash: labels.get(LABEL_HASH).cloned(),
            });
        }

        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }
}

pub(crate) fn build_container_config(
    manifest: &DeploymentManifest,
    spec_hash: &str,
) -> Config<String> {
    let container = manifest.container();

    let exposed_ports = if container.ports.is_empty() {
        None
    } else {
        Some(
            container
                .ports
                .iter()
                .map(|port| (port.docker_port_key(), HashMap::new()))
                .collect(),
        )
    };

    let port_bindings = build_port_bindings(&container.ports);
    let host_config = (!port_bindings.is_empty()).then(|| HostConfig {
        port_bindings: Some(port_bindings),
        ..Default::default()
    });

    let labels = HashMap::from([
        (LABEL_MANAGED.to_string(), "true".to_string()),
        (
            LABEL_DEPLOYMENT.to_string(),
            manifest.deployment_name().to_string(),
        ),
        (LABEL_HASH.to_string(), spec_hash.to_string()),
    ]);

    Config {
        image: Some(container.image.clone()),
        entrypoint: (!container.command.is_empty()).then_some(container.command.clone()),
        cmd: (!container.args.is_empty()).then_some(container.args.clone()),
        env: (!container.env.is_empty()).then_some(
            container
                .env
                .iter()
                .map(|entry| format!("{}={}", entry.name, entry.value))
                .collect(),
        ),
        exposed_ports,
        host_config,
        labels: Some(labels),
        ..Default::default()
    }
}

fn build_port_bindings(ports: &[ContainerPort]) -> HashMap<String, Option<Vec<PortBinding>>> {
    ports
        .iter()
        .filter_map(|port| {
            port.host_port.map(|host_port| {
                (
                    port.docker_port_key(),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: Some(host_port.to_string()),
                    }]),
                )
            })
        })
        .collect()
}

fn connect_docker() -> Result<Docker> {
    if let Ok(host) = env::var("DOCKER_HOST") {
        return connect_from_host(&host);
    }

    for candidate in socket_candidates() {
        if candidate.exists() {
            return Docker::connect_with_unix(
                candidate.to_string_lossy().as_ref(),
                120,
                API_DEFAULT_VERSION,
            )
            .with_context(|| format!("failed to connect to Docker on {}", candidate.display()));
        }
    }

    bail!("failed to locate a usable Docker socket; set DOCKER_HOST or ensure Docker is running")
}

fn connect_from_host(host: &str) -> Result<Docker> {
    if let Some(path) = host.strip_prefix("unix://") {
        return Docker::connect_with_unix(path, 120, API_DEFAULT_VERSION)
            .with_context(|| format!("failed to connect to Docker on {host}"));
    }

    if host.starts_with("tcp://") || host.starts_with("http://") || host.starts_with("https://") {
        return Docker::connect_with_http(host, 120, API_DEFAULT_VERSION)
            .with_context(|| format!("failed to connect to Docker on {host}"));
    }

    bail!("unsupported DOCKER_HOST scheme in `{host}`")
}

fn socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".orbstack/run/docker.sock"));
        candidates.push(home.join(".docker/run/docker.sock"));
    }
    candidates.push(Path::new("/var/run/docker.sock").to_path_buf());
    candidates
}

pub fn deployment_container_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|char| match char {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => char,
            _ => '-',
        })
        .collect::<String>();
    format!("litek8s-{sanitized}")
}

#[cfg(test)]
mod tests {
    use crate::manifest::{API_VERSION, DeploymentManifest};

    use super::{LABEL_DEPLOYMENT, LABEL_HASH, build_container_config, deployment_container_name};

    #[test]
    fn creates_expected_container_config() {
        let manifest = DeploymentManifest::parse(&format!(
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
        ))
        .expect("manifest should parse");

        let config = build_container_config(&manifest, "hash123");
        assert_eq!(config.image.as_deref(), Some("nginx:alpine"));
        assert_eq!(
            config.entrypoint.as_ref().expect("entrypoint missing"),
            &vec!["nginx".to_string()]
        );
        assert_eq!(
            config.cmd.as_ref().expect("cmd missing"),
            &vec!["-g".to_string(), "daemon off;".to_string()]
        );

        let labels = config.labels.expect("labels missing");
        assert_eq!(
            labels.get(LABEL_DEPLOYMENT).map(String::as_str),
            Some("demo")
        );
        assert_eq!(labels.get(LABEL_HASH).map(String::as_str), Some("hash123"));

        let bindings = config
            .host_config
            .expect("host config missing")
            .port_bindings
            .expect("port bindings missing");
        assert!(bindings.contains_key("80/tcp"));
    }

    #[test]
    fn sanitizes_container_name() {
        assert_eq!(
            deployment_container_name("api/v1.demo"),
            "litek8s-api-v1.demo"
        );
    }
}
