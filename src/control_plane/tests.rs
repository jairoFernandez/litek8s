use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::control_plane::raft::ControlPlaneNode;
use crate::control_plane::{ClusterCommand, CommandResult};
use crate::store::{DeploymentStatus, EventLevel, Store};

/// Create an isolated single-node Raft cluster backed by a temp state file.
async fn spawn_raft_node() -> (Arc<Store>, ControlPlaneNode, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let state_path = tmp.path().join("state.json");
    let store = Arc::new(Store::load(state_path).await.unwrap());

    // Use port 0 — the RPC listener is not started for these tests so the
    // address is never actually bound.  We only need the Raft engine itself.
    let node = ControlPlaneNode::start(1, "127.0.0.1:0".into(), Arc::clone(&store), None)
        .await
        .unwrap();
    (store, node, tmp)
}

/// Helper: build a minimal valid deployment YAML.
fn deployment_yaml(name: &str) -> String {
    format!(
        r#"apiVersion: litek8s.dev/v1alpha1
kind: Deployment
metadata:
  name: {name}
spec:
  replicas: 1
  template:
    spec:
      restartPolicy: Always
      containers:
        - name: app
          image: alpine:latest
"#
    )
}

/// Percentile helper (requires a sorted slice).
fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    let idx = ((sorted.len() as f64) * pct / 100.0).ceil() as usize;
    sorted[idx.saturating_sub(1).min(sorted.len() - 1)]
}

fn print_latency_stats(label: &str, durations: &mut Vec<Duration>) {
    durations.sort();
    let total: Duration = durations.iter().sum();
    let avg = total / durations.len() as u32;
    let p50 = percentile(durations, 50.0);
    let p99 = percentile(durations, 99.0);
    let max = *durations.last().unwrap();
    eprintln!(
        "  {label}: n={}, avg={avg:?}, p50={p50:?}, p99={p99:?}, max={max:?}",
        durations.len()
    );
}

// -------------------------------------------------------------------------
// Performance tests
// -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_raft_propose_set_status_latency() {
    let (store, node, _tmp) = spawn_raft_node().await;

    // Seed a deployment so SetDeploymentStatus has something to mutate.
    let yaml = deployment_yaml("perf-status");
    node.propose(ClusterCommand::ApplyDeployment {
        manifest_yaml: yaml,
    })
    .await
    .unwrap();

    // Warm up the Raft engine (first proposals may include leader election).
    for _ in 0..10 {
        node.propose(ClusterCommand::SetDeploymentStatus {
            name: "perf-status".into(),
            status: DeploymentStatus::Running,
            ready: true,
            container_id: Some("warm".into()),
            message: Some("warmup".into()),
            last_exit: None,
            increment_restart_count: false,
            increment_restart_streak: false,
            next_retry_at: None,
        })
        .await
        .unwrap();
    }

    // Measure 200 sequential proposes.
    let iterations = 200;
    let mut latencies = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let start = Instant::now();
        let result = node
            .propose(ClusterCommand::SetDeploymentStatus {
                name: "perf-status".into(),
                status: DeploymentStatus::Running,
                ready: true,
                container_id: Some(format!("cid-{i}")),
                message: Some(format!("iteration {i}")),
                last_exit: None,
                increment_restart_count: false,
                increment_restart_streak: false,
                next_retry_at: None,
            })
            .await
            .unwrap();
        latencies.push(start.elapsed());
        assert!(matches!(result, CommandResult::Changed { .. }));
    }

    eprintln!("\n--- Raft propose (SetDeploymentStatus) latency ---");
    print_latency_stats("propose", &mut latencies);

    // Compare: direct store writes (same operation, bypassing Raft).
    let mut direct_latencies = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let start = Instant::now();
        store
            .set_status(
                "perf-status",
                DeploymentStatus::Running,
                true,
                Some(format!("direct-{i}")),
                Some(format!("direct {i}")),
                None,
                false,
                false,
                None,
            )
            .await
            .unwrap();
        direct_latencies.push(start.elapsed());
    }

    eprintln!("--- Direct store write (SetDeploymentStatus) latency ---");
    print_latency_stats("direct", &mut direct_latencies);

    let avg_propose: Duration = latencies.iter().sum::<Duration>() / iterations as u32;
    let avg_direct: Duration = direct_latencies.iter().sum::<Duration>() / iterations as u32;
    let overhead = avg_propose.as_nanos() as f64 / avg_direct.as_nanos().max(1) as f64;
    eprintln!("  overhead ratio: {overhead:.1}x");

    // Single-node Raft should add less than 10x overhead over a direct write.
    assert!(
        overhead < 10.0,
        "raft overhead {overhead:.1}x exceeds 10x threshold"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_raft_propose_event_throughput() {
    let (_store, node, _tmp) = spawn_raft_node().await;

    // Warm up.
    for _ in 0..5 {
        node.propose(ClusterCommand::AppendEvent {
            level: EventLevel::Normal,
            reason: "Warmup".into(),
            object_kind: "deployment".into(),
            object_name: "perf".into(),
            node_name: None,
            message: "warming up".into(),
        })
        .await
        .unwrap();
    }

    let iterations = 500;
    let start = Instant::now();
    for i in 0..iterations {
        node.propose(ClusterCommand::AppendEvent {
            level: EventLevel::Normal,
            reason: "PerfTest".into(),
            object_kind: "deployment".into(),
            object_name: format!("perf-{i}"),
            node_name: Some("local".into()),
            message: format!("event number {i}"),
        })
        .await
        .unwrap();
    }
    let elapsed = start.elapsed();
    let throughput = iterations as f64 / elapsed.as_secs_f64();

    eprintln!("\n--- Raft propose (AppendEvent) throughput ---");
    eprintln!(
        "  {iterations} events in {elapsed:?} — {throughput:.0} ops/sec"
    );

    // Single-node Raft should sustain at least 500 ops/sec for events.
    assert!(
        throughput > 500.0,
        "event throughput {throughput:.0} ops/sec is below 500 ops/sec threshold"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_raft_propose_mixed_workload() {
    let (_store, node, _tmp) = spawn_raft_node().await;

    // Seed deployments.
    for i in 0..5 {
        node.propose(ClusterCommand::ApplyDeployment {
            manifest_yaml: deployment_yaml(&format!("mix-{i}")),
        })
        .await
        .unwrap();
    }

    // Simulate a realistic reconciliation cycle: status updates + events
    // interleaved, as the reconciler would produce them.
    let cycles = 50;
    let start = Instant::now();
    for cycle in 0..cycles {
        for dep in 0..5 {
            let name = format!("mix-{dep}");
            node.propose(ClusterCommand::SetDeploymentStatus {
                name: name.clone(),
                status: DeploymentStatus::Running,
                ready: true,
                container_id: Some(format!("c-{dep}-{cycle}")),
                message: Some("healthy".into()),
                last_exit: None,
                increment_restart_count: false,
                increment_restart_streak: false,
                next_retry_at: None,
            })
            .await
            .unwrap();

            node.propose(ClusterCommand::AppendEvent {
                level: EventLevel::Normal,
                reason: "Running".into(),
                object_kind: "deployment".into(),
                object_name: name,
                node_name: Some("local".into()),
                message: "container is healthy".into(),
            })
            .await
            .unwrap();
        }
    }
    let elapsed = start.elapsed();
    let total_ops = cycles * 5 * 2; // 2 proposes per deployment per cycle
    let throughput = total_ops as f64 / elapsed.as_secs_f64();
    let avg = elapsed / total_ops as u32;

    eprintln!("\n--- Raft mixed workload (reconcile simulation) ---");
    eprintln!(
        "  {total_ops} ops ({cycles} cycles × 5 deployments × 2 ops) in {elapsed:?}"
    );
    eprintln!("  avg={avg:?}, throughput={throughput:.0} ops/sec");

    // Mixed workload should sustain at least 400 ops/sec.
    assert!(
        throughput > 400.0,
        "mixed throughput {throughput:.0} ops/sec is below 400 ops/sec threshold"
    );
}
