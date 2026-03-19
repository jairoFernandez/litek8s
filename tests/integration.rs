use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use litek8s::daemon::{ServerConfig, serve};
use litek8s::manifest::API_VERSION;
use litek8s::protocol::{DeploymentDescription, Request, Response, request};
use litek8s::runtime::docker::DockerRuntime;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_get_logs_and_delete_roundtrip() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("roundtrip");
    let manifest = manifest_yaml(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "echo ready && sleep 3600"]"#,
    );

    match request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?
    {
        Response::Ack { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    harness.wait_until_running(&deployment).await?;

    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => {
            assert_eq!(item.name, deployment);
            assert_eq!(item.image, test_image());
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let logs = request(
        &harness.socket_path,
        Request::Logs {
            name: deployment.clone(),
            tail: 20,
        },
    )
    .await?;
    match logs {
        Response::Logs { content } => assert!(content.contains("ready")),
        Response::Error { message } if message.contains("failed to fetch Docker logs") => {}
        other => panic!("unexpected response: {other:?}"),
    }

    let top = request(&harness.socket_path, Request::Top).await?;
    match top {
        Response::Top { daemon, items } => {
            assert!(daemon.pid > 0);
            assert!(daemon.memory_bytes > 0);
            let row = items
                .into_iter()
                .find(|item| item.name == deployment)
                .expect("deployment should appear in top");
            assert_eq!(row.status, "Running");
            assert!(row.container_id.is_some());
            assert_eq!(row.restart_count, 0);
            assert!(row.last_exit.is_none());
        }
        other => panic!("unexpected response: {other:?}"),
    }

    match request(
        &harness.socket_path,
        Request::DeleteDeployment {
            name: deployment.clone(),
        },
    )
    .await?
    {
        Response::Ack { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    let list = request(&harness.socket_path, Request::GetDeployments).await?;
    match list {
        Response::Deployments { items } => {
            assert!(items.into_iter().all(|item| item.name != deployment));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolves_proxy_target_for_published_port() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("proxy");
    let manifest =
        manifest_yaml_with_host_port(&deployment, r#"["sh"]"#, r#"["-c", "sleep 3600"]"#, 18090);

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    harness.wait_until_running(&deployment).await?;

    let response = request(
        &harness.socket_path,
        Request::ResolveProxyTarget {
            name: deployment.clone(),
            port: None,
        },
    )
    .await?;

    match response {
        Response::ProxyTarget { item } => {
            assert_eq!(item.deployment_name, deployment);
            assert_eq!(item.node_name, "local");
            assert_eq!(item.transport, "local");
            assert_eq!(item.container_port, 8080);
            assert_eq!(item.host_port, 18090);
            assert_eq!(item.protocol, "TCP");
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_listener_forwards_to_deployment() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("svc-echo");
    let service = unique_name("svc");
    let manifest = http_echo_manifest(&deployment, 18094, "hello through service");
    let service_manifest = service_manifest_yaml(&service, &deployment, "127.0.0.1:19094", None);

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    harness.wait_until_running(&deployment).await?;

    let add_service = request(
        &harness.socket_path,
        Request::Apply {
            manifest: service_manifest.clone(),
        },
    )
    .await?;
    match add_service {
        Response::Ack { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    let describe = harness
        .wait_until_service_status(&service, |item| item.status == "Listening")
        .await?;
    assert_eq!(describe.listen_addr, "127.0.0.1:19094");

    let mut client = tokio::net::TcpStream::connect("127.0.0.1:19094").await?;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    client.shutdown().await?;
    let mut response = Vec::new();
    client.read_to_end(&mut response).await?;
    let body = String::from_utf8_lossy(&response);
    assert!(body.contains("hello through service"));

    match request(
        &harness.socket_path,
        Request::DeleteManifest {
            manifest: service_manifest,
        },
    )
    .await?
    {
        Response::Ack { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciler_recreates_container_after_manual_removal() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("restart");
    let manifest = manifest_yaml(&deployment, r#"["sh"]"#, r#"["-c", "sleep 3600"]"#);

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    let original_id = harness.wait_until_running(&deployment).await?;

    let runtime = DockerRuntime::connect()?;
    runtime.remove_deployment(&deployment).await?;

    let new_id = harness
        .wait_until_container_id_changes(&deployment, &original_id)
        .await?;
    assert_ne!(original_id, new_id);

    let top = request(&harness.socket_path, Request::Top).await?;
    match top {
        Response::Top { items, .. } => {
            let row = items
                .into_iter()
                .find(|item| item.name == deployment)
                .expect("deployment should appear in top");
            assert!(row.restart_count >= 1);
            assert!(matches!(
                row.last_exit.as_deref(),
                Some("removed") | Some("container disappeared")
            ));
            assert_eq!(row.container_id.as_deref(), Some(new_id.as_str()));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_restart_rehydrates_state_file() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let mut harness = TestHarness::spawn().await?;
    let deployment = unique_name("rehydrate");
    let manifest = manifest_yaml(&deployment, r#"["sh"]"#, r#"["-c", "sleep 3600"]"#);

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    harness.wait_until_running(&deployment).await?;

    harness.restart().await?;
    let running_id = harness.wait_until_running(&deployment).await?;

    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => {
            assert_eq!(item.status, "Running");
            assert_eq!(item.container_id.as_deref(), Some(running_id.as_str()));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applies_and_updates_container_resource_limits() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("resources");
    let runtime = DockerRuntime::connect()?;

    let initial_manifest = manifest_yaml_with_limits(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "sleep 3600"]"#,
        Some("250m"),
        Some("64Mi"),
    );

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: initial_manifest,
        },
    )
    .await?;
    let original_id = harness.wait_until_running(&deployment).await?;

    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => {
            assert_eq!(item.cpu_limit.as_deref(), Some("250m"));
            assert_eq!(item.memory_limit.as_deref(), Some("64Mi"));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let limits = runtime
        .configured_limits_for_deployment(&deployment)
        .await?;
    assert_eq!(limits.nano_cpus, Some(250_000_000));
    assert_eq!(limits.memory_bytes, Some(67_108_864));

    let updated_manifest = manifest_yaml_with_limits(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "sleep 3600"]"#,
        Some("500m"),
        Some("128Mi"),
    );

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: updated_manifest,
        },
    )
    .await?;
    let updated_id = harness
        .wait_until_container_id_changes(&deployment, &original_id)
        .await?;
    assert_ne!(original_id, updated_id);

    let updated_limits = runtime
        .configured_limits_for_deployment(&deployment)
        .await?;
    assert_eq!(updated_limits.nano_cpus, Some(500_000_000));
    assert_eq!(updated_limits.memory_bytes, Some(134_217_728));

    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => {
            assert!(item.restart_count >= 1);
            assert!(matches!(
                item.last_exit.as_deref(),
                Some("spec changed") | Some("removed")
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_loop_uses_restart_backoff() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("backoff");
    let manifest = manifest_yaml(&deployment, r#"["sh"]"#, r#"["-c", "exit 1"]"#);

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;

    let item = harness
        .wait_until_description(&deployment, |item| {
            item.status == "Restarting"
                && item.container_id.is_none()
                && item
                    .message
                    .as_deref()
                    .map(|message| message.contains("backing off 4s"))
                    .unwrap_or(false)
        })
        .await?;

    assert!(item.restart_count >= 3);
    assert!(item.restart_streak >= 3);
    assert!(matches!(
        item.last_exit.as_deref(),
        Some("exited(1)") | Some("removed")
    ));
    assert!(item.next_retry_in_secs.unwrap_or_default() > 0);

    tokio::time::sleep(Duration::from_millis(250)).await;
    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => {
            assert_eq!(item.status, "Restarting");
            assert!(item.container_id.is_none());
            assert!(item.restart_streak >= 3);
            assert!(item.next_retry_in_secs.unwrap_or_default() > 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cordon_blocks_new_default_scheduling_until_uncordoned() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("cordon");
    let manifest = manifest_yaml(&deployment, r#"["sh"]"#, r#"["-c", "sleep 3600"]"#);

    let cordon = request(
        &harness.socket_path,
        Request::SetNodeScheduling {
            name: "local".to_string(),
            unschedulable: true,
        },
    )
    .await?;
    match cordon {
        Response::Ack { message } => assert!(message.contains("cordoned")),
        other => panic!("unexpected response: {other:?}"),
    }

    let apply = request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    match apply {
        Response::Error { message } => {
            assert!(message.contains("no schedulable node is available"));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let nodes = request(&harness.socket_path, Request::GetNodes).await?;
    match nodes {
        Response::Nodes { items } => {
            let local = items
                .into_iter()
                .find(|item| item.name == "local")
                .expect("local node should exist");
            assert_eq!(local.scheduling, "Cordoned");
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let uncordon = request(
        &harness.socket_path,
        Request::SetNodeScheduling {
            name: "local".to_string(),
            unschedulable: false,
        },
    )
    .await?;
    match uncordon {
        Response::Ack { message } => assert!(message.contains("uncordoned")),
        other => panic!("unexpected response: {other:?}"),
    }

    let apply = request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    match apply {
        Response::Ack { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    harness.wait_until_running(&deployment).await?;
    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_can_filter_by_deployment_and_node() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("events");
    let manifest = manifest_yaml(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "echo ready && sleep 3600"]"#,
    );

    request(
        &harness.socket_path,
        Request::Apply {
            manifest: manifest.clone(),
        },
    )
    .await?;
    harness.wait_until_running(&deployment).await?;

    let deployment_events = request(
        &harness.socket_path,
        Request::GetEvents {
            limit: 50,
            since_id: None,
            deployment_name: Some(deployment.clone()),
            node_name: None,
        },
    )
    .await?;
    match deployment_events {
        Response::Events { items } => {
            assert!(!items.is_empty());
            assert!(
                items
                    .iter()
                    .all(|item| item.object_kind == "deployment" && item.object_name == deployment)
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let node_events = request(
        &harness.socket_path,
        Request::GetEvents {
            limit: 50,
            since_id: None,
            deployment_name: Some(deployment.clone()),
            node_name: Some("local".to_string()),
        },
    )
    .await?;
    match node_events {
        Response::Events { items } => {
            assert!(!items.is_empty());
            assert!(items.iter().all(|item| item.object_name == deployment));
            assert!(
                items
                    .iter()
                    .all(|item| item.node_name.as_deref() == Some("local"))
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

struct TestHarness {
    _temp_dir: TempDir,
    socket_path: PathBuf,
    state_file: PathBuf,
    server: tokio::task::JoinHandle<Result<()>>,
}

impl TestHarness {
    async fn spawn() -> Result<Self> {
        let temp_dir = tempfile::tempdir().context("failed to create temp dir")?;
        let socket_path = temp_dir.path().join("daemon.sock");
        let state_file = temp_dir.path().join("state.json");
        let server = spawn_server(socket_path.clone(), state_file.clone()).await?;
        Ok(Self {
            _temp_dir: temp_dir,
            socket_path,
            state_file,
            server,
        })
    }

    async fn restart(&mut self) -> Result<()> {
        self.server.abort();
        let _ = (&mut self.server).await;
        if self.socket_path.exists() {
            tokio::fs::remove_file(&self.socket_path).await.ok();
        }
        self.server = spawn_server(self.socket_path.clone(), self.state_file.clone()).await?;
        Ok(())
    }

    async fn wait_until_running(&self, deployment: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let response = request(
                &self.socket_path,
                Request::DescribeDeployment {
                    name: deployment.to_string(),
                },
            )
            .await?;

            if let Response::Description { item } = response {
                if item.status == "Running" {
                    return item
                        .container_id
                        .context("deployment reported Running without a container ID");
                }
            }

            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("deployment `{deployment}` did not become Running in time");
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn cleanup_deployment(&self, deployment: &str) -> Result<()> {
        let _ = request(
            &self.socket_path,
            Request::DeleteDeployment {
                name: deployment.to_string(),
            },
        )
        .await;

        let runtime = DockerRuntime::connect()?;
        let _ = runtime.remove_deployment(deployment).await;
        Ok(())
    }

    async fn wait_until_description<F>(
        &self,
        deployment: &str,
        mut predicate: F,
    ) -> Result<DeploymentDescription>
    where
        F: FnMut(&DeploymentDescription) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let response = request(
                &self.socket_path,
                Request::DescribeDeployment {
                    name: deployment.to_string(),
                },
            )
            .await?;

            if let Response::Description { item } = response {
                if predicate(&item) {
                    return Ok(item);
                }
            }

            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("deployment `{deployment}` did not reach the expected description");
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait_until_container_id_changes(
        &self,
        deployment: &str,
        previous_id: &str,
    ) -> Result<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let current_id = self.wait_until_running(deployment).await?;
            if current_id != previous_id {
                return Ok(current_id);
            }

            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("deployment `{deployment}` did not recreate its container in time");
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait_until_service_status<F>(
        &self,
        service: &str,
        mut predicate: F,
    ) -> Result<litek8s::protocol::ServiceDescription>
    where
        F: FnMut(&litek8s::protocol::ServiceDescription) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let response = request(
                &self.socket_path,
                Request::DescribeService {
                    name: service.to_string(),
                },
            )
            .await?;

            if let Response::ServiceDescription { item } = response {
                if predicate(&item) {
                    return Ok(item);
                }
            }

            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("service `{service}` did not reach the expected status");
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn spawn_server(
    socket_path: PathBuf,
    state_file: PathBuf,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        state_file,
        poll_interval: Duration::from_millis(500),
        cleanup_orphans: false,
    };
    let handle = tokio::spawn(async move { serve(config).await });
    wait_for_socket(&socket_path).await?;
    Ok(handle)
}

async fn wait_for_socket(socket_path: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("daemon socket was not created in time");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn docker_available() -> bool {
    match DockerRuntime::connect() {
        Ok(runtime) => runtime.ping().await.is_ok(),
        Err(_) => false,
    }
}

fn test_image() -> String {
    env::var("LITEK8S_TEST_IMAGE").unwrap_or_else(|_| "busybox:1.36".to_string())
}

fn manifest_yaml(deployment: &str, command: &str, args: &str) -> String {
    manifest_yaml_with_limits(deployment, command, args, None, None)
}

fn manifest_yaml_with_limits(
    deployment: &str,
    command: &str,
    args: &str,
    cpu_limit: Option<&str>,
    memory_limit: Option<&str>,
) -> String {
    let resources = match (cpu_limit, memory_limit) {
        (None, None) => String::new(),
        _ => {
            let mut block = String::from("          resources:\n            limits:\n");
            if let Some(cpu_limit) = cpu_limit {
                block.push_str(&format!("              cpu: \"{cpu_limit}\"\n"));
            }
            if let Some(memory_limit) = memory_limit {
                block.push_str(&format!("              memory: \"{memory_limit}\"\n"));
            }
            block
        }
    };

    format!(
        r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {deployment}
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: app
          image: {image}
          command: {command}
          args: {args}
{resources}
"#,
        image = test_image(),
        resources = resources,
    )
}

fn manifest_yaml_with_host_port(
    deployment: &str,
    command: &str,
    args: &str,
    host_port: u16,
) -> String {
    format!(
        r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {deployment}
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: app
          image: {image}
          command: {command}
          args: {args}
          ports:
            - containerPort: 8080
              hostPort: {host_port}
"#,
        image = test_image(),
        command = command,
        args = args,
    )
}

fn http_echo_manifest(deployment: &str, host_port: u16, text: &str) -> String {
    format!(
        r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {deployment}
  labels:
    app: {deployment}
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: echo
          image: hashicorp/http-echo:1.0.0
          args:
            - "-listen"
            - ":5678"
            - "-text"
            - "{text}"
          ports:
            - containerPort: 5678
              hostPort: {host_port}
"#
    )
}

fn service_manifest_yaml(
    service: &str,
    deployment: &str,
    listen_addr: &str,
    target_port: Option<u16>,
) -> String {
    let port = listen_addr
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .expect("listen_addr must end with a port");
    let target_port = target_port.unwrap_or(5678);

    format!(
        r#"
apiVersion: {API_VERSION}
kind: Service
metadata:
  name: {service}
spec:
  selector:
    app: {deployment}
  ports:
    - port: {port}
      targetPort: {target_port}
"#
    )
}

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

fn integration_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
