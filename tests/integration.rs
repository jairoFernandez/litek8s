use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use litek8s::daemon::{ServerConfig, serve};
use litek8s::manifest::API_VERSION;
use litek8s::protocol::{Request, Response, request};
use litek8s::runtime::docker::DockerRuntime;
use tempfile::TempDir;

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

    let new_id = harness.wait_until_running(&deployment).await?;
    assert_ne!(original_id, new_id);

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

    let describe = request(
        &harness.socket_path,
        Request::DescribeDeployment {
            name: deployment.clone(),
        },
    )
    .await?;
    match describe {
        Response::Description { item } => assert_eq!(item.status, "Running"),
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
}

async fn spawn_server(
    socket_path: PathBuf,
    state_file: PathBuf,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        state_file,
        poll_interval: Duration::from_millis(500),
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
"#,
        image = test_image(),
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
