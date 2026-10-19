use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU16, Ordering};
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

    // Running means the container started, not that `echo ready` has run yet:
    // poll the logs briefly instead of reading them once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let logs = request(
            &harness.socket_path,
            Request::Logs {
                name: deployment.clone(),
                tail: 20,
            },
        )
        .await?;
        match logs {
            Response::Logs { content } if content.contains("ready") => break,
            Response::Logs { content } if tokio::time::Instant::now() >= deadline => {
                panic!("logs never contained `ready`: {content:?}")
            }
            Response::Logs { .. } => tokio::time::sleep(Duration::from_millis(200)).await,
            Response::Error { message } if message.contains("failed to fetch Docker logs") => break,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    let top = request(&harness.socket_path, Request::Top).await?;
    match top {
        Response::Top { daemon, items, .. } => {
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
async fn replicas_three_creates_three_containers() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let mut harness = TestHarness::spawn().await?;
    let deployment = unique_name("replicas3");
    let manifest = manifest_yaml_with_replicas(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "sleep 3600"]"#,
        3,
    );
    harness.apply(&manifest).await?;

    let ids = harness.wait_until_replicas_ready(&deployment, 3).await?;
    assert_eq!(ids.len(), 3, "expected 3 replica container ids");

    let names = harness.replica_container_names(&deployment).await?;
    assert_eq!(
        names,
        vec![
            format!("litek8s-{deployment}-0"),
            format!("litek8s-{deployment}-1"),
            format!("litek8s-{deployment}-2"),
        ]
    );

    harness.stop_and_cleanup(&deployment).await?;
    Ok(())
}

// Guards against the event-driven reconcile storm: every Docker create/remove
// wakes the loop, and a healthy replica transiently listed as not-running used
// to be recreated, churning its container ID. Covered by event debouncing in
// the reconcile loop and transient-state tolerance in the reconciler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scaling_up_and_down_keeps_surviving_replica_ids() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let mut harness = TestHarness::spawn().await?;
    let deployment = unique_name("scale");
    let cmd = r#"["sh"]"#;
    let args = r#"["-c", "sleep 3600"]"#;

    harness
        .apply(&manifest_yaml_with_replicas(&deployment, cmd, args, 3))
        .await?;
    let ids3 = harness.wait_until_replicas_ready(&deployment, 3).await?;

    // Scale up 3 -> 5: ordinals 0..2 must keep the SAME container IDs (no storm).
    harness
        .apply(&manifest_yaml_with_replicas(&deployment, cmd, args, 5))
        .await?;
    let ids5 = harness.wait_until_replicas_ready(&deployment, 5).await?;
    for ordinal in 0..3 {
        assert_eq!(
            ids3.get(&ordinal),
            ids5.get(&ordinal),
            "ordinal {ordinal} should not be recreated on scale-up"
        );
    }

    // Scale down 5 -> 2: ordinals 0..1 survive with same IDs, 2..4 are gone.
    harness
        .apply(&manifest_yaml_with_replicas(&deployment, cmd, args, 2))
        .await?;
    let ids2 = harness.wait_until_replicas_ready(&deployment, 2).await?;
    for ordinal in 0..2 {
        assert_eq!(
            ids5.get(&ordinal),
            ids2.get(&ordinal),
            "ordinal {ordinal} should survive scale-down unchanged"
        );
    }
    let names = harness.wait_until_container_count(&deployment, 2).await?;
    assert_eq!(
        names,
        vec![
            format!("litek8s-{deployment}-0"),
            format!("litek8s-{deployment}-1"),
        ],
        "scaled-down ordinals should be removed"
    );

    harness.stop_and_cleanup(&deployment).await?;
    Ok(())
}

// Killing one replica triggers an event flood; the other replicas' container
// IDs must not churn (see `scaling_up_and_down_keeps_surviving_replica_ids`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_replica_is_recreated() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let mut harness = TestHarness::spawn().await?;
    let deployment = unique_name("revive");
    harness
        .apply(&manifest_yaml_with_replicas(
            &deployment,
            r#"["sh"]"#,
            r#"["-c", "sleep 3600"]"#,
            3,
        ))
        .await?;
    let ids = harness.wait_until_replicas_ready(&deployment, 3).await?;

    // Kill ordinal 1's container directly via Docker.
    let runtime = DockerRuntime::connect()?;
    let victim = ids.get(&1).expect("ordinal 1 id").clone();
    runtime.remove_container(&victim).await?;

    // Reconciler should bring ready_replicas back to 3 with a NEW id for ord 1.
    let revived = harness.wait_until_replicas_ready(&deployment, 3).await?;
    assert_ne!(
        revived.get(&1),
        Some(&victim),
        "ordinal 1 should be recreated with a new container id"
    );
    assert_eq!(revived.get(&0), ids.get(&0), "ordinal 0 untouched");
    assert_eq!(revived.get(&2), ids.get(&2), "ordinal 2 untouched");

    harness.stop_and_cleanup(&deployment).await?;
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
    let host_port = unique_port(18_090);
    let manifest = manifest_yaml_with_host_port(
        &deployment,
        r#"["sh"]"#,
        r#"["-c", "sleep 3600"]"#,
        host_port,
    );

    harness.apply(&manifest).await?;
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
            assert_eq!(item.host_port, Some(host_port));
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
    let host_port = unique_port(18_094);
    let service_port = unique_port(19_094);
    let listen_addr = format!("127.0.0.1:{service_port}");
    let manifest =
        http_echo_manifest_with_readiness(&deployment, host_port, "hello through service");
    let service_manifest = service_manifest_yaml(&service, &deployment, &listen_addr, None);

    harness.apply(&manifest).await?;
    harness
        .wait_until_description(&deployment, |item| item.status == "Ready" && item.ready)
        .await?;

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
    assert_eq!(describe.listen_addr, listen_addr);

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", service_port)).await?;
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
async fn deployment_with_readiness_probe_becomes_ready() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("ready");
    let host_port = unique_port(18_096);
    let manifest = http_echo_manifest_with_readiness(&deployment, host_port, "ready with probe");

    harness.apply(&manifest).await?;

    let describe = harness
        .wait_until_description(&deployment, |item| item.status == "Ready" && item.ready)
        .await?;
    assert_eq!(
        describe.readiness_probe.as_deref(),
        Some("tcpSocket:5678 (initialDelay=0s timeout=1s)")
    );
    assert!(
        describe
            .conditions
            .iter()
            .any(|condition| condition.type_name == "Ready" && condition.status == "True")
    );

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_waits_for_readiness_before_exposing_endpoints() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("not-ready");
    let service = unique_name("not-ready-svc");
    let host_port = unique_port(18_097);
    let service_port = unique_port(19_097);
    let manifest = sleeping_manifest_with_host_port_and_readiness(&deployment, host_port, 8080);
    let service_manifest = service_manifest_yaml(
        &service,
        &deployment,
        &format!("127.0.0.1:{service_port}"),
        Some(8080),
    );

    harness.apply(&manifest).await?;

    let describe = harness
        .wait_until_description(&deployment, |item| {
            item.status == "Running"
                && !item.ready
                && item
                    .message
                    .as_deref()
                    .map(|message| message.contains("readiness probe is still failing"))
                    .unwrap_or(false)
        })
        .await?;
    assert!(!describe.ready);

    harness.apply(&service_manifest).await?;

    let service_describe = harness
        .wait_until_service_status(&service, |item| item.status == "NoEndpoints")
        .await?;
    assert!(
        service_describe
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("deployment")
    );

    harness.cleanup_deployment(&deployment).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_transitions_to_no_endpoints_after_backend_disappears() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("svc-lost");
    let service = unique_name("svc-lost");
    let host_port = unique_port(18_095);
    let service_port = unique_port(19_095);
    let manifest = http_echo_manifest(&deployment, host_port, "hello through service");
    let service_manifest = service_manifest_yaml(
        &service,
        &deployment,
        &format!("127.0.0.1:{service_port}"),
        None,
    );

    harness.apply(&manifest).await?;
    harness.wait_until_running(&deployment).await?;

    harness.apply(&service_manifest).await?;
    harness
        .wait_until_service_status(&service, |item| item.status == "Listening")
        .await?;

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

    let describe = harness
        .wait_until_service_status(&service, |item| item.status == "NoEndpoints")
        .await?;
    assert_eq!(
        describe.deployment_name,
        format!("selector:app={deployment}")
    );
    assert!(
        describe
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("no deployment matches selector")
    );

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

    harness.apply(&manifest).await?;
    let original_id = harness.wait_until_running(&deployment).await?;

    let runtime = DockerRuntime::connect()?;
    runtime.remove_deployment(&deployment).await?;

    let new_id = harness
        .wait_until_container_id_changes(&deployment, &original_id)
        .await?;
    assert_ne!(original_id, new_id);

    let row = wait_for(
        "top to show the recreated container",
        wait_timeout(60),
        Duration::from_millis(200),
        || request(&harness.socket_path, Request::Top),
        |top| match top {
            Response::Top { items, .. } => {
                let row = items
                    .into_iter()
                    .find(|item| item.name == deployment)
                    .expect("deployment should appear in top");
                if row.restart_count >= 1 && row.container_id.as_deref() == Some(new_id.as_str()) {
                    Probe::Done(row)
                } else {
                    Probe::Pending(format!(
                        "restarts={} container={:?}",
                        row.restart_count, row.container_id
                    ))
                }
            }
            other => panic!("unexpected response: {other:?}"),
        },
    )
    .await?;
    assert!(matches!(
        row.last_exit.as_deref(),
        Some("removed") | Some("container disappeared")
    ));

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

    harness.apply(&manifest).await?;
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

    harness.apply(&initial_manifest).await?;
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

    harness.apply(&updated_manifest).await?;
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

    harness.apply(&manifest).await?;

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
async fn drain_without_alternative_cordons_node_and_reports_blocked_workloads() -> Result<()> {
    let _guard = integration_lock().lock().await;
    if !docker_available().await {
        return Ok(());
    }

    let harness = TestHarness::spawn().await?;
    let deployment = unique_name("drain");
    let manifest = manifest_yaml(&deployment, r#"["sh"]"#, r#"["-c", "sleep 3600"]"#);

    harness.apply(&manifest).await?;
    harness.wait_until_running(&deployment).await?;

    let drain = request(
        &harness.socket_path,
        Request::DrainNode {
            name: "local".to_string(),
        },
    )
    .await?;
    match drain {
        Response::Ack { message } => {
            assert!(message.contains("drained"));
            assert!(message.contains("blocked"));
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

    let item = harness
        .wait_until_description(&deployment, |item| {
            item.node_name == "local" && matches!(item.status.as_str(), "Running" | "Ready")
        })
        .await?;
    assert_eq!(item.node_name, "local");

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

    harness.apply(&manifest).await?;
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

    /// Stop the daemon, then remove the deployment's containers directly. Used by
    /// multi-replica tests so the background reconciler can't recreate a replica
    /// while we tear down (which would leak long-lived containers into the next
    /// test sharing the Docker host).
    async fn stop_and_cleanup(&mut self, deployment: &str) -> Result<()> {
        self.server.abort();
        let _ = (&mut self.server).await;
        let runtime = DockerRuntime::connect()?;
        // Reconcile may have created the container just before we aborted; retry.
        for _ in 0..10 {
            runtime.remove_deployment(deployment).await.ok();
            if runtime
                .list_containers_for_deployment(deployment)
                .await?
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    }

    async fn apply(&self, manifest: &str) -> Result<()> {
        match request(
            &self.socket_path,
            Request::Apply {
                manifest: manifest.to_string(),
            },
        )
        .await?
        {
            Response::Ack { .. } => Ok(()),
            other => anyhow::bail!("unexpected apply response: {other:?}"),
        }
    }

    /// Names of all managed containers for a deployment, via Docker directly.
    async fn replica_container_names(&self, deployment: &str) -> Result<Vec<String>> {
        let runtime = DockerRuntime::connect()?;
        let mut names: Vec<String> = runtime
            .list_containers_for_deployment(deployment)
            .await?
            .into_iter()
            .map(|c| c.name)
            .collect();
        names.sort();
        Ok(names)
    }

    // Waits on the daemon. Each one polls through `wait_for`, which logs
    // progress on slow machines and fails with the last state it observed;
    // deployment waits also attach the deployment's recent events.

    async fn describe(&self, deployment: &str) -> Result<Option<DeploymentDescription>> {
        match request(
            &self.socket_path,
            Request::DescribeDeployment {
                name: deployment.to_string(),
            },
        )
        .await?
        {
            Response::Description { item } => Ok(Some(item)),
            Response::Error { .. } => Ok(None),
            other => anyhow::bail!("unexpected describe response: {other:?}"),
        }
    }

    /// The deployment's most recent events, one per line, for timeout errors.
    async fn recent_events(&self, deployment: &str) -> String {
        let response = request(
            &self.socket_path,
            Request::GetEvents {
                limit: 15,
                since_id: None,
                deployment_name: Some(deployment.to_string()),
                node_name: None,
            },
        )
        .await;
        match response {
            Ok(Response::Events { items }) if !items.is_empty() => items
                .iter()
                .map(|e| format!("  {} {}: {}", e.level, e.reason, e.message))
                .collect::<Vec<_>>()
                .join("\n"),
            Ok(Response::Events { .. }) => "  (none)".to_string(),
            Ok(other) => format!("  (unexpected response: {other:?})"),
            Err(err) => format!("  (could not fetch events: {err:#})"),
        }
    }

    async fn wait_until_description<F>(
        &self,
        deployment: &str,
        mut predicate: F,
    ) -> Result<DeploymentDescription>
    where
        F: FnMut(&DeploymentDescription) -> bool,
    {
        let result = wait_for(
            &format!("deployment `{deployment}` to reach the expected state"),
            wait_timeout(180),
            Duration::from_millis(500),
            || self.describe(deployment),
            |item| match item {
                Some(item) if predicate(&item) => Probe::Done(item),
                Some(item) => Probe::Pending(summarize(&item)),
                None => Probe::Pending("deployment not found".to_string()),
            },
        )
        .await;
        match result {
            Ok(item) => Ok(item),
            Err(err) => Err(anyhow::anyhow!(
                "{err:#}\nrecent events of `{deployment}`:\n{}",
                self.recent_events(deployment).await
            )),
        }
    }

    async fn wait_until_running(&self, deployment: &str) -> Result<String> {
        let item = self
            .wait_until_description(deployment, |item| item.status == "Running")
            .await?;
        item.container_id
            .context("deployment reported Running without a container ID")
    }

    async fn wait_until_container_id_changes(
        &self,
        deployment: &str,
        previous_id: &str,
    ) -> Result<String> {
        let item = self
            .wait_until_description(deployment, |item| {
                item.status == "Running"
                    && item
                        .container_id
                        .as_deref()
                        .is_some_and(|id| id != previous_id)
            })
            .await?;
        Ok(item.container_id.expect("checked by the predicate"))
    }

    /// Wait until the deployment reports `ready_replicas == desired_replicas`
    /// at the expected count, returning the per-ordinal container IDs.
    async fn wait_until_replicas_ready(
        &self,
        deployment: &str,
        expected: u32,
    ) -> Result<std::collections::BTreeMap<u32, String>> {
        // Wait for the store to report all replicas ready...
        self.wait_until_description(deployment, |item| {
            item.desired_replicas == expected
                && item.ready_replicas == expected
                && item.replicas.len() as u32 == expected
                && item.replicas.iter().all(|r| r.container_id.is_some())
        })
        .await?;

        // ...then read the ground-truth ordinal->id map from Docker, and require
        // it to hold steady across two polls so a mid-reconcile recreation can't
        // leak a transient id into the assertions.
        let runtime = DockerRuntime::connect()?;
        let mut previous: Option<std::collections::BTreeMap<u32, String>> = None;
        wait_for(
            &format!("deployment `{deployment}` to settle at {expected} running replicas"),
            wait_timeout(120),
            Duration::from_millis(600),
            || runtime.list_containers_for_deployment(deployment),
            |containers| {
                let mut map = std::collections::BTreeMap::new();
                for c in containers {
                    // Only running containers count; exited ones (mid-recreate)
                    // would otherwise shadow the live container for an ordinal.
                    if c.running {
                        if let Some(ordinal) = c.ordinal {
                            map.insert(ordinal, c.id);
                        }
                    }
                }
                let settled = map.len() as u32 == expected
                    && previous.as_ref() == Some(&map)
                    && map.keys().copied().eq(0..expected);
                if settled {
                    return Probe::Done(map);
                }
                let state = format!("running replicas {map:?}");
                previous = Some(map);
                Probe::Pending(state)
            },
        )
        .await
    }

    /// Wait until exactly `expected` managed containers exist for the deployment
    /// (so scale-down has fully reaped the extras), returning their sorted names.
    async fn wait_until_container_count(
        &self,
        deployment: &str,
        expected: usize,
    ) -> Result<Vec<String>> {
        wait_for(
            &format!("deployment `{deployment}` to settle at {expected} containers"),
            wait_timeout(120),
            Duration::from_millis(500),
            || self.replica_container_names(deployment),
            |names| {
                if names.len() == expected {
                    Probe::Done(names)
                } else {
                    Probe::Pending(format!("containers {names:?}"))
                }
            },
        )
        .await
    }

    async fn wait_until_service_status<F>(
        &self,
        service: &str,
        mut predicate: F,
    ) -> Result<litek8s::protocol::ServiceDescription>
    where
        F: FnMut(&litek8s::protocol::ServiceDescription) -> bool,
    {
        wait_for(
            &format!("service `{service}` to reach the expected status"),
            wait_timeout(60),
            Duration::from_millis(250),
            || {
                request(
                    &self.socket_path,
                    Request::DescribeService {
                        name: service.to_string(),
                    },
                )
            },
            |response| match response {
                Response::ServiceDescription { item } if predicate(&item) => Probe::Done(item),
                Response::ServiceDescription { item } => {
                    Probe::Pending(format!("status={} message={:?}", item.status, item.message))
                }
                other => Probe::Pending(format!("{other:?}")),
            },
        )
        .await
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// One observation of a [`wait_for`] loop.
enum Probe<T> {
    Done(T),
    /// Not there yet; says what was seen, for progress lines and the timeout error.
    Pending(String),
}

/// Deadline for a wait, `base_secs` scaled by `LITEK8S_TEST_TIMEOUT_SCALE`.
/// The bases are generous on purpose: on a loaded machine (a CI runner, a VM
/// soak next door) pulling and starting a container can take a minute. A real
/// hang still fails, with the last state `wait_for` observed.
fn wait_timeout(base_secs: u64) -> Duration {
    let scale = env::var("LITEK8S_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0);
    Duration::from_secs_f64(base_secs as f64 * scale)
}

const WAIT_PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// Poll `fetch` every `interval` until `check` accepts what it returned.
/// Errors from `fetch` (the daemon went away) fail at once. While waiting, a
/// progress line with the last observed state goes to stderr every 15 s, and
/// the timeout error carries that state too.
async fn wait_for<S, T, Fetch, Fut, Check>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut fetch: Fetch,
    mut check: Check,
) -> Result<T>
where
    Fetch: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<S>>,
    Check: FnMut(S) -> Probe<T>,
{
    let start = tokio::time::Instant::now();
    let mut next_report = start + WAIT_PROGRESS_EVERY;
    loop {
        let last = match check(fetch().await?) {
            Probe::Done(value) => {
                if start.elapsed() >= WAIT_PROGRESS_EVERY {
                    eprintln!("[wait] {what}: done after {:.1?}", start.elapsed());
                }
                return Ok(value);
            }
            Probe::Pending(state) => state,
        };
        let now = tokio::time::Instant::now();
        if now - start > timeout {
            anyhow::bail!("timed out after {timeout:?} waiting for {what}; last observed: {last}");
        }
        if now >= next_report {
            eprintln!(
                "[wait] {what}: still waiting after {:.0?}; last observed: {last}",
                now - start
            );
            next_report += WAIT_PROGRESS_EVERY;
        }
        tokio::time::sleep(interval).await;
    }
}

fn summarize(item: &DeploymentDescription) -> String {
    format!(
        "status={} ready={} ready_replicas={}/{} container={:?} restarts={} last_exit={:?} message={:?}",
        item.status,
        item.ready,
        item.ready_replicas,
        item.desired_replicas,
        item.container_id
            .as_deref()
            .map(|id| &id[..id.len().min(12)]),
        item.restart_count,
        item.last_exit,
        item.message,
    )
}

async fn spawn_server(
    socket_path: PathBuf,
    state_file: PathBuf,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let config = ServerConfig {
        socket_path: socket_path.clone(),
        state_file,
        poll_interval: Duration::from_millis(500),
        node_resource_interval: Duration::from_secs(5),
        cleanup_orphans: false,
        peer_id: 1,
        peer_addr: "0.0.0.0:0".into(),
        join_addr: None,
        api_addr: None,
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

fn manifest_yaml_with_replicas(
    deployment: &str,
    command: &str,
    args: &str,
    replicas: u32,
) -> String {
    format!(
        r#"
apiVersion: {API_VERSION}
kind: Deployment
metadata:
  name: {deployment}
spec:
  replicas: {replicas}
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

fn http_echo_manifest_with_readiness(deployment: &str, host_port: u16, text: &str) -> String {
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
          readinessProbe:
            tcpSocket:
              port: 5678
"#
    )
}

fn sleeping_manifest_with_host_port_and_readiness(
    deployment: &str,
    host_port: u16,
    probe_port: u16,
) -> String {
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
        - name: app
          image: {image}
          command: ["sh"]
          args: ["-c", "sleep 3600"]
          ports:
            - containerPort: 8080
              hostPort: {host_port}
          readinessProbe:
            tcpSocket:
              port: {probe_port}
"#,
        image = test_image(),
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

fn unique_port(base: u16) -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(0);
    let offset = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    base.saturating_add(offset % 500)
}

fn integration_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
