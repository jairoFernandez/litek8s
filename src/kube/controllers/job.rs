//! Job: run pods until `completions` succeed or `backoffLimit` failures.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const INDEX_ANNOTATION: &str = "batch.kubernetes.io/job-completion-index";

fn finished(job: &Value) -> bool {
    util::condition_true(job, "Complete") || util::condition_true(job, "Failed")
}

fn pod_index(pod: &Value) -> Option<i64> {
    pod.pointer("/metadata/annotations")
        .and_then(|a| a.get(INDEX_ANNOTATION))
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
}

/// `0,2-4` for {0, 2, 3, 4}.
fn format_indexes(set: &BTreeSet<i64>) -> String {
    let mut parts = Vec::new();
    let mut iter = set.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap();
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
    }
    parts.join(",")
}

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(job) = api.get_object("batch", "jobs", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&job) {
        return Ok(None);
    }
    let now = util::now();
    if finished(&job) {
        return ttl_after_finished(api, &job);
    }

    let selector = crate::kube::selector::LabelSelector::from_object(job.pointer("/spec/selector"));
    let pods = super::replicaset::adopt_and_release(api, ns, &job, selector.as_ref())?;
    let indexed = s(&job, "/spec/completionMode") == "Indexed";
    let completions = i(&job, "/spec/completions");
    let parallelism = i(&job, "/spec/parallelism").unwrap_or(1).max(0);
    let backoff_limit = i(&job, "/spec/backoffLimit").unwrap_or(6);
    let per_index_limit = i(&job, "/spec/backoffLimitPerIndex");
    let max_failed_indexes = i(&job, "/spec/maxFailedIndexes");
    let suspended = job.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);

    // podFailurePolicy: the first matching rule decides what a failed pod
    // means (fail the job, fail its index, ignore it, or count it).
    let mut fail_job: Option<String> = None;
    let mut failed = 0i64;
    let mut failed_per_index: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    let mut failed_indexes: BTreeSet<i64> = BTreeSet::new();
    for pod in pods.iter().filter(|p| util::phase(p) == "Failed") {
        match failure_action(&job, pod) {
            Some(("FailJob", rule)) => {
                fail_job.get_or_insert(format!("Container {} for pod {}/{} failed with exit code {} matching FailJob rule at index {rule}",
                    failed_container(pod).0, ns, util::name(pod), failed_container(pod).1));
                failed += 1;
            }
            Some(("Ignore", _)) => {}
            Some(("FailIndex", _)) => {
                failed += 1;
                if let Some(idx) = pod_index(pod) {
                    failed_indexes.insert(idx);
                }
            }
            _ => {
                failed += 1;
                if let Some(idx) = pod_index(pod) {
                    *failed_per_index.entry(idx).or_default() += 1;
                }
            }
        }
    }
    if let Some(limit) = per_index_limit {
        for (idx, n) in &failed_per_index {
            if *n > limit {
                failed_indexes.insert(*idx);
            }
        }
    }
    let succeeded_pods: Vec<&Value> = pods.iter().filter(|p| util::phase(p) == "Succeeded").collect();
    let active: Vec<&Value> = pods.iter().filter(|p| util::is_active(p)).collect();

    let succeeded_indexes: BTreeSet<i64> = succeeded_pods.iter().filter_map(|p| pod_index(p)).collect();
    let succeeded = if indexed { succeeded_indexes.len() as i64 } else { succeeded_pods.len() as i64 };
    let success_policy_met = indexed && success_policy_met(&job, &succeeded_indexes);
    let mut status = job.get("status").cloned().unwrap_or_else(|| json!({}));
    if !status.is_object() {
        status = json!({});
    }
    if status.get("startTime").is_none() && !suspended {
        status["startTime"] = json!(util::rfc3339(now));
    }
    let mut conditions = util::conditions_of(&job);

    // With per-index backoff, the job ends once every index succeeded or failed.
    let all_indexes_done = per_index_limit.is_some()
        && completions.is_some_and(|c| (0..c).all(|idx| succeeded_indexes.contains(&idx) || failed_indexes.contains(&idx)));
    let complete = success_policy_met
        || (failed_indexes.is_empty()
            && match completions {
                Some(c) => succeeded >= c,
                None => succeeded >= 1 && active.is_empty(),
            });
    let too_many_failed_indexes = max_failed_indexes.is_some_and(|m| failed_indexes.len() as i64 > m);
    let failed_by_indexes = !failed_indexes.is_empty() && (all_indexes_done || too_many_failed_indexes);
    let deadline_exceeded = i(&job, "/spec/activeDeadlineSeconds").is_some_and(|d| {
        status
            .get("startTime")
            .and_then(Value::as_str)
            .and_then(util::parse_time)
            .is_some_and(|start| (now - start).num_seconds() >= d)
    });
    let mut requeue = None;

    if complete {
        for pod in &active {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
        let (reason, message) = if success_policy_met {
            ("SuccessPolicy", "Matched rules at index 0")
        } else {
            ("CompletionsReached", "Reached expected number of succeeded pods")
        };
        util::set_condition(&mut conditions, "SuccessCriteriaMet", "True", reason, message);
        util::set_condition(&mut conditions, "Complete", "True", reason, message);
        status["completionTime"] = json!(util::rfc3339(now));
        status["active"] = json!(0);
        status["ready"] = json!(0);
    } else if fail_job.is_some() || (per_index_limit.is_none() && failed > backoff_limit) || deadline_exceeded || failed_by_indexes {
        for pod in &active {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
        let (reason, message) = if let Some(msg) = &fail_job {
            ("PodFailurePolicy", msg.as_str())
        } else if deadline_exceeded {
            ("DeadlineExceeded", "Job was active longer than specified deadline")
        } else if too_many_failed_indexes {
            ("MaxFailedIndexesExceeded", "Job has exceeded the specified maximal number of failed indexes")
        } else if failed_by_indexes {
            ("FailedIndexes", "Job has failed indexes")
        } else {
            ("BackoffLimitExceeded", "Job has reached the specified backoff limit")
        };
        util::set_condition(&mut conditions, "FailureTarget", "True", reason, message);
        util::set_condition(&mut conditions, "Failed", "True", reason, message);
        status["active"] = json!(0);
        status["ready"] = json!(0);
    } else if suspended {
        for pod in &active {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
        util::set_condition(&mut conditions, "Suspended", "True", "JobSuspended", "Job suspended");
        status["active"] = json!(0);
    } else {
        if util::condition_true(&job, "Suspended") {
            util::set_condition(&mut conditions, "Suspended", "False", "JobResumed", "Job resumed");
        }
        // Back off after failures (10s, 20s, ... up to 6 minutes), measured
        // from the most recent failure.
        let backoff = if failed > 0 && per_index_limit.is_none() {
            let delay = (10i64 << (failed - 1).min(5)).min(360);
            let last_failure = pods
                .iter()
                .filter(|p| util::phase(p) == "Failed")
                .filter_map(|p| last_finish(p))
                .max();
            last_failure
                .map(|t| delay - (now - t).num_seconds())
                .filter(|left| *left > 0)
        } else {
            None
        };
        if let Some(left) = backoff {
            requeue = Some(Duration::from_secs(left as u64));
        } else {
            let remaining = completions.map(|c| c - succeeded - failed_indexes.len() as i64).unwrap_or(parallelism);
            let want = parallelism.min(remaining).max(0);
            let missing = want - active.len() as i64;
            if missing > 0 {
                let template = job.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
                let mut free_indexes = (0..completions.unwrap_or(0)).filter(|idx| {
                    !succeeded_indexes.contains(idx) && !failed_indexes.contains(idx) && !active.iter().any(|p| pod_index(p) == Some(*idx))
                });
                for _ in 0..missing {
                    let mut pod = util::pod_from_template(&template, &job, &[]);
                    if indexed {
                        let Some(idx) = free_indexes.next() else { break };
                        pod["metadata"]["generateName"] = json!(format!("{name}-{idx}-"));
                        pod["metadata"]["labels"][INDEX_ANNOTATION] = json!(idx.to_string());
                        if !pod["metadata"].get("annotations").is_some_and(Value::is_object) {
                            pod["metadata"]["annotations"] = json!({});
                        }
                        pod["metadata"]["annotations"][INDEX_ANNOTATION] = json!(idx.to_string());
                        pod["spec"]["hostname"] = json!(format!("{name}-{idx}"));
                        add_env(&mut pod, "JOB_COMPLETION_INDEX", &idx.to_string());
                    } else {
                        pod["metadata"]["generateName"] = json!(format!("{name}-"));
                    }
                    api.create_object("", "pods", Some(ns), pod)?;
                }
            }
        }
        status["active"] = json!(active.len());
        status["ready"] = json!(active.iter().filter(|p| util::is_ready(p)).count());
    }
    status["succeeded"] = json!(succeeded);
    status["failed"] = json!(failed);
    status["terminating"] = json!(0);
    status["uncountedTerminatedPods"] = json!({});
    if indexed {
        status["completedIndexes"] = json!(format_indexes(&succeeded_indexes));
    }
    if per_index_limit.is_some() || !failed_indexes.is_empty() {
        status["failedIndexes"] = json!(format_indexes(&failed_indexes));
    }
    if conditions.is_empty() {
        if let Some(m) = status.as_object_mut() {
            m.remove("conditions");
        }
    } else {
        status["conditions"] = json!(conditions);
    }
    util::write_status(api, "batch", "jobs", &job, status)?;
    Ok(requeue)
}

/// The (container, exit code) of a failed pod's first failed container.
fn failed_container(pod: &Value) -> (String, i64) {
    for c in pod.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten() {
        if let Some(code) = c.pointer("/state/terminated/exitCode").and_then(Value::as_i64).filter(|c| *c != 0) {
            return (s(c, "/name").to_string(), code);
        }
    }
    (String::new(), 0)
}

/// The action of the first podFailurePolicy rule a failed pod matches, and
/// that rule's index.
fn failure_action<'a>(job: &'a Value, pod: &Value) -> Option<(&'a str, usize)> {
    let rules = job.pointer("/spec/podFailurePolicy/rules")?.as_array()?;
    let statuses: Vec<&Value> = pod.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten().collect();
    for (n, rule) in rules.iter().enumerate() {
        let action = rule.get("action").and_then(Value::as_str).unwrap_or_default();
        if let Some(on) = rule.get("onExitCodes") {
            let values: Vec<i64> = on.get("values").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_i64).collect();
            let container = s(on, "/containerName");
            let not_in = s(on, "/operator") == "NotIn";
            let hit = statuses.iter().filter(|c| container.is_empty() || s(c, "/name") == container).any(|c| {
                match c.pointer("/state/terminated/exitCode").and_then(Value::as_i64) {
                    Some(0) | None => false,
                    Some(code) => values.contains(&code) != not_in,
                }
            });
            if hit {
                return Some((action, n));
            }
        }
        if let Some(conds) = rule.get("onPodConditions").and_then(Value::as_array) {
            let hit = conds.iter().any(|want| {
                util::conditions_of(pod).iter().any(|c| c["type"] == want["type"] && c["status"] == *want.get("status").unwrap_or(&json!("True")))
            });
            if hit {
                return Some((action, n));
            }
        }
    }
    None
}

/// Whether any successPolicy rule is satisfied by the succeeded indexes.
fn success_policy_met(job: &Value, succeeded: &BTreeSet<i64>) -> bool {
    let Some(rules) = job.pointer("/spec/successPolicy/rules").and_then(Value::as_array) else { return false };
    rules.iter().any(|rule| {
        let wanted: Option<BTreeSet<i64>> = rule.get("succeededIndexes").and_then(Value::as_str).map(parse_indexes);
        let count = rule.get("succeededCount").and_then(Value::as_i64);
        let matched: i64 = match &wanted {
            Some(set) => set.intersection(succeeded).count() as i64,
            None => succeeded.len() as i64,
        };
        match (wanted, count) {
            (Some(set), None) => set.is_subset(succeeded),
            (_, Some(c)) => matched >= c,
            (None, None) => false,
        }
    })
}

/// `0,2-4` -> {0, 2, 3, 4}.
fn parse_indexes(raw: &str) -> BTreeSet<i64> {
    let mut out = BTreeSet::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                    out.extend(a..=b);
                }
            }
            None => {
                if let Ok(v) = part.parse() {
                    out.insert(v);
                }
            }
        }
    }
    out
}

fn last_finish(pod: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    pod.pointer("/status/containerStatuses")?
        .as_array()?
        .iter()
        .filter_map(|c| c.pointer("/state/terminated/finishedAt").and_then(Value::as_str))
        .filter_map(util::parse_time)
        .max()
}

fn add_env(pod: &mut Value, name: &str, value: &str) {
    for field in ["initContainers", "containers"] {
        if let Some(cs) = pod["spec"].get_mut(field).and_then(Value::as_array_mut) {
            for c in cs {
                if !c.get("env").is_some_and(Value::is_array) {
                    c["env"] = json!([]);
                }
                c["env"].as_array_mut().unwrap().push(json!({"name": name, "value": value}));
            }
        }
    }
}

fn ttl_after_finished(api: &ApiState, job: &Value) -> Result<Requeue, ApiError> {
    let Some(ttl) = i(job, "/spec/ttlSecondsAfterFinished") else {
        return Ok(None);
    };
    let finished_at = ["/status/completionTime"]
        .iter()
        .find_map(|p| job.pointer(p).and_then(Value::as_str).and_then(util::parse_time))
        .or_else(|| {
            util::conditions_of(job)
                .iter()
                .filter(|c| c["type"] == "Failed" || c["type"] == "Complete")
                .filter_map(|c| c.get("lastTransitionTime").and_then(Value::as_str).and_then(util::parse_time))
                .max()
        })
        .unwrap_or_else(util::now);
    let left = ttl - (util::now() - finished_at).num_seconds();
    if left > 0 {
        return Ok(Some(Duration::from_secs(left as u64)));
    }
    let (ns, name) = util::ns_name(job);
    api.delete_named("batch", "jobs", &ns, &name)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_index_ranges() {
        let set: BTreeSet<i64> = [0, 2, 3, 4, 7].into_iter().collect();
        assert_eq!(format_indexes(&set), "0,2-4,7");
        assert_eq!(parse_indexes("0,2-4,7"), set);
        assert_eq!(parse_indexes(" 1 , x, 3-a,5"), [1, 5].into_iter().collect());
        assert_eq!(format_indexes(&BTreeSet::new()), "");
    }

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn create_job(api: &ApiState, spec: Value) -> Value {
        let mut spec = spec;
        spec["template"] = json!({"spec": {"restartPolicy": "Never", "containers": [{"name": "main", "image": "busybox"}]}});
        let job = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "pi"}, "spec": spec});
        api.create_object("batch", "jobs", Some("default"), job).unwrap()
    }

    fn job(api: &ApiState) -> Value {
        api.get_object("batch", "jobs", "default", "pi").unwrap()
    }

    fn set_spec(api: &ApiState, f: impl FnOnce(&mut Value)) {
        let mut j = job(api);
        f(&mut j["spec"]);
        api.update_object("batch", "jobs", j).unwrap();
    }

    fn reconcile_job(api: &ApiState) -> Requeue {
        reconcile(api, "default", "pi").unwrap()
    }

    fn active_pods(api: &ApiState) -> Vec<Value> {
        let mut pods: Vec<Value> = api.store().list("", "pods", Some("default")).0.into_iter().filter(util::is_active).collect();
        pods.sort_by_key(|p| (pod_index(p), util::name(p)));
        pods
    }

    fn terminate(api: &ApiState, pod: &Value, exit_code: i64, extra: Value) {
        let phase = if exit_code == 0 { "Succeeded" } else { "Failed" };
        let mut terminated = json!({"exitCode": exit_code});
        if let Some(t) = extra.get("finishedAt") {
            terminated["finishedAt"] = t.clone();
        }
        let mut status = json!({"phase": phase, "containerStatuses": [{"name": "main", "state": {"terminated": terminated}}]});
        if let Some(c) = extra.get("conditions") {
            status["conditions"] = c.clone();
        }
        util::write_status(api, "", "pods", pod, status).unwrap();
    }

    fn succeed(api: &ApiState, pod: &Value) {
        terminate(api, pod, 0, json!({}));
    }

    fn fail(api: &ApiState, pod: &Value, exit_code: i64) {
        terminate(api, pod, exit_code, json!({}));
    }

    fn condition_of(job: &Value, type_: &str) -> Option<(String, String)> {
        util::condition(job, type_).map(|c| (s(c, "/status").to_string(), s(c, "/reason").to_string()))
    }

    #[test]
    fn runs_parallelism_pods_until_completions_succeed() {
        let api = api();
        let created = create_job(&api, json!({"completions": 3, "parallelism": 2}));
        assert_eq!(reconcile_job(&api), None);
        let pods = active_pods(&api);
        assert_eq!(pods.len(), 2);
        for p in &pods {
            assert!(util::name(p).starts_with("pi-"));
            assert_eq!(s(p, "/metadata/labels/job-name"), "pi");
            assert!(util::controlled_by(p, &util::uid(&created)));
        }
        let st = job(&api)["status"].clone();
        assert!(st["startTime"].is_string());
        assert!(st.get("completedIndexes").is_none());

        reconcile_job(&api);
        assert_eq!(job(&api)["status"]["active"], 2);
        succeed(&api, &pods[0]);
        reconcile_job(&api);
        // One left to run, one running: still two at a time at most.
        let pods = active_pods(&api);
        assert_eq!(pods.len(), 2);
        assert_eq!(job(&api)["status"]["succeeded"], 1);

        succeed(&api, &pods[0]);
        reconcile_job(&api);
        // Two succeeded, one running: no new pod.
        assert_eq!(active_pods(&api).len(), 1);
        succeed(&api, &active_pods(&api)[0]);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Complete"), Some(("True".into(), "CompletionsReached".into())));
        assert_eq!(condition_of(&j, "SuccessCriteriaMet"), Some(("True".into(), "CompletionsReached".into())));
        assert_eq!(j["status"]["succeeded"], 3);
        assert_eq!(j["status"]["active"], 0);
        assert!(j["status"]["completionTime"].is_string());
        // Finished: nothing more happens and no TTL means no requeue.
        assert_eq!(reconcile_job(&api), None);
        assert!(active_pods(&api).is_empty());
    }

    #[test]
    fn completion_removes_pods_still_running() {
        let api = api();
        create_job(&api, json!({"completions": 1, "parallelism": 2}));
        reconcile_job(&api);
        // parallelism is capped by the completions left.
        let pods = active_pods(&api);
        assert_eq!(pods.len(), 1);
        succeed(&api, &pods[0]);
        api.create_object("", "pods", Some("default"), {
            let mut extra = util::pod_from_template(&job(&api)["spec"]["template"], &job(&api), &[]);
            extra["metadata"]["name"] = json!("pi-extra");
            extra
        })
        .unwrap();
        reconcile_job(&api);
        assert!(util::condition_true(&job(&api), "Complete"));
        assert!(active_pods(&api).is_empty());
    }

    #[test]
    fn work_queue_completes_when_one_succeeded_and_none_run() {
        let api = api();
        create_job(&api, json!({"parallelism": 2}));
        assert!(job(&api).pointer("/spec/completions").is_none());
        reconcile_job(&api);
        let pods = active_pods(&api);
        assert_eq!(pods.len(), 2);
        succeed(&api, &pods[0]);
        reconcile_job(&api);
        // The other worker still runs.
        assert!(!util::condition_true(&job(&api), "Complete"));
        assert!(!active_pods(&api).is_empty());
        for p in active_pods(&api) {
            succeed(&api, &p);
        }
        reconcile_job(&api);
        assert!(util::condition_true(&job(&api), "Complete"));
        assert!(active_pods(&api).is_empty());
    }

    #[test]
    fn backoff_limit_fails_the_job() {
        let api = api();
        create_job(&api, json!({"backoffLimit": 1}));
        reconcile_job(&api);
        fail(&api, &active_pods(&api)[0], 1);
        reconcile_job(&api);
        // One failure is within the limit: a replacement runs.
        assert_eq!(active_pods(&api).len(), 1);
        assert_eq!(job(&api)["status"]["failed"], 1);
        assert!(!util::condition_true(&job(&api), "Failed"));
        fail(&api, &active_pods(&api)[0], 1);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Failed"), Some(("True".into(), "BackoffLimitExceeded".into())));
        assert_eq!(condition_of(&j, "FailureTarget"), Some(("True".into(), "BackoffLimitExceeded".into())));
        assert_eq!(j["status"]["failed"], 2);
        assert!(active_pods(&api).is_empty());
        assert!(j["status"].get("completionTime").is_none());
    }

    #[test]
    fn waits_out_the_backoff_after_a_recent_failure() {
        let api = api();
        create_job(&api, json!({}));
        reconcile_job(&api);
        let finished = util::rfc3339(util::now());
        terminate(&api, &active_pods(&api)[0], 1, json!({"finishedAt": finished}));
        let requeue = reconcile_job(&api).expect("requeued for the backoff");
        assert!(requeue <= Duration::from_secs(10) && requeue >= Duration::from_secs(8), "{requeue:?}");
        assert!(active_pods(&api).is_empty());

        // A failure longer ago than the delay is retried at once.
        let api = self::api();
        create_job(&api, json!({}));
        reconcile_job(&api);
        let finished = util::rfc3339(util::now() - chrono::Duration::seconds(30));
        terminate(&api, &active_pods(&api)[0], 1, json!({"finishedAt": finished}));
        assert_eq!(reconcile_job(&api), None);
        assert_eq!(active_pods(&api).len(), 1);
    }

    #[test]
    fn pod_failure_policy_fail_job_matches_exit_codes() {
        let api = api();
        create_job(&api, json!({"backoffLimit": 6, "podFailurePolicy": {"rules": [
            {"action": "FailJob", "onExitCodes": {"containerName": "main", "operator": "In", "values": [42]}},
        ]}}));
        reconcile_job(&api);
        // Exit code 1 matches no rule and counts toward the backoff limit.
        fail(&api, &active_pods(&api)[0], 1);
        reconcile_job(&api);
        assert!(!util::condition_true(&job(&api), "Failed"));
        let pod = active_pods(&api)[0].clone();
        fail(&api, &pod, 42);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Failed"), Some(("True".into(), "PodFailurePolicy".into())));
        let message = s(util::condition(&j, "Failed").unwrap(), "/message").to_string();
        assert_eq!(
            message,
            format!("Container main for pod default/{} failed with exit code 42 matching FailJob rule at index 0", util::name(&pod))
        );
        assert_eq!(j["status"]["failed"], 2);
    }

    #[test]
    fn pod_failure_policy_ignores_matching_failures() {
        let api = api();
        create_job(&api, json!({"backoffLimit": 0, "podFailurePolicy": {"rules": [
            {"action": "Ignore", "onPodConditions": [{"type": "DisruptionTarget"}]},
            {"action": "Ignore", "onExitCodes": {"operator": "NotIn", "values": [1, 2]}},
            {"action": "Count", "onExitCodes": {"containerName": "other", "operator": "In", "values": [1]}},
        ]}}));
        reconcile_job(&api);
        terminate(&api, &active_pods(&api)[0], 1, json!({"conditions": [{"type": "DisruptionTarget", "status": "True"}]}));
        reconcile_job(&api);
        // Not counted: backoffLimit 0 is not exceeded and the pod is replaced.
        assert_eq!(job(&api)["status"]["failed"], 0);
        assert_eq!(active_pods(&api).len(), 1);
        fail(&api, &active_pods(&api)[0], 7);
        reconcile_job(&api);
        assert_eq!(job(&api)["status"]["failed"], 0);
        assert_eq!(active_pods(&api).len(), 1);
        // Exit code 2 matches no rule (the Count rule names another container).
        fail(&api, &active_pods(&api)[0], 2);
        reconcile_job(&api);
        assert_eq!(condition_of(&job(&api), "Failed"), Some(("True".into(), "BackoffLimitExceeded".into())));
    }

    #[test]
    fn indexed_pods_carry_their_completion_index() {
        let api = api();
        create_job(&api, json!({"completions": 3, "parallelism": 2, "completionMode": "Indexed"}));
        reconcile_job(&api);
        let pods = active_pods(&api);
        assert_eq!(pods.iter().map(pod_index).collect::<Vec<_>>(), [Some(0), Some(1)]);
        for (idx, p) in pods.iter().enumerate() {
            assert!(util::name(p).starts_with(&format!("pi-{idx}-")));
            assert_eq!(s(p, "/spec/hostname"), format!("pi-{idx}"));
            assert_eq!(s(p, "/metadata/labels/batch.kubernetes.io~1job-completion-index"), idx.to_string());
            let env = &p["spec"]["containers"][0]["env"];
            assert_eq!(env[0], json!({"name": "JOB_COMPLETION_INDEX", "value": idx.to_string()}));
        }
        succeed(&api, &pods[1]);
        reconcile_job(&api);
        // Index 1 is done, index 0 runs: the free index 2 starts.
        assert_eq!(active_pods(&api).iter().map(pod_index).collect::<Vec<_>>(), [Some(0), Some(2)]);
        assert_eq!(job(&api)["status"]["completedIndexes"], "1");

        // A failed index is retried under the same index.
        let pods = active_pods(&api);
        fail(&api, &pods[0], 1);
        succeed(&api, &pods[1]);
        reconcile_job(&api);
        assert_eq!(active_pods(&api).iter().map(pod_index).collect::<Vec<_>>(), [Some(0)]);
        assert_eq!(job(&api)["status"]["completedIndexes"], "1-2");
        succeed(&api, &active_pods(&api)[0]);
        reconcile_job(&api);
        let j = job(&api);
        assert!(util::condition_true(&j, "Complete"));
        assert_eq!(j["status"]["completedIndexes"], "0-2");
        assert_eq!(j["status"]["succeeded"], 3);
        assert_eq!(j["status"]["failed"], 1);
        assert!(j["status"].get("failedIndexes").is_none());
    }

    #[test]
    fn backoff_limit_per_index_fails_the_index_then_the_job() {
        let api = api();
        create_job(&api, json!({"completions": 2, "parallelism": 2, "completionMode": "Indexed", "backoffLimitPerIndex": 0}));
        reconcile_job(&api);
        let pods = active_pods(&api);
        fail(&api, &pods[0], 1);
        reconcile_job(&api);
        // Index 0 used up its retries; index 1 still runs, so the job goes on.
        let j = job(&api);
        assert_eq!(j["status"]["failedIndexes"], "0");
        assert!(!util::condition_true(&j, "Failed"));
        assert_eq!(active_pods(&api).iter().map(pod_index).collect::<Vec<_>>(), [Some(1)]);
        succeed(&api, &pods[1]);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Failed"), Some(("True".into(), "FailedIndexes".into())));
        assert_eq!(j["status"]["completedIndexes"], "1");
        assert!(!util::condition_true(&j, "Complete"));
    }

    #[test]
    fn max_failed_indexes_fails_the_job_early() {
        let api = api();
        create_job(&api, json!({"completions": 3, "parallelism": 3, "completionMode": "Indexed",
            "backoffLimitPerIndex": 1, "maxFailedIndexes": 0}));
        reconcile_job(&api);
        let pod0 = active_pods(&api)[0].clone();
        fail(&api, &pod0, 1);
        reconcile_job(&api);
        // One retry per index: index 0 runs again.
        assert_eq!(active_pods(&api).iter().map(pod_index).collect::<Vec<_>>(), [Some(0), Some(1), Some(2)]);
        assert_eq!(job(&api)["status"]["failedIndexes"], "");
        fail(&api, &active_pods(&api)[0], 1);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Failed"), Some(("True".into(), "MaxFailedIndexesExceeded".into())));
        assert_eq!(j["status"]["failedIndexes"], "0");
        assert!(active_pods(&api).is_empty());
    }

    #[test]
    fn fail_index_rule_fails_only_that_index() {
        let api = api();
        create_job(&api, json!({"completions": 2, "parallelism": 2, "completionMode": "Indexed", "backoffLimitPerIndex": 3,
            "podFailurePolicy": {"rules": [{"action": "FailIndex", "onExitCodes": {"operator": "In", "values": [3]}}]}}));
        reconcile_job(&api);
        let pods = active_pods(&api);
        fail(&api, &pods[1], 3);
        reconcile_job(&api);
        assert_eq!(job(&api)["status"]["failedIndexes"], "1");
        assert_eq!(active_pods(&api).iter().map(pod_index).collect::<Vec<_>>(), [Some(0)]);
        succeed(&api, &pods[0]);
        reconcile_job(&api);
        assert_eq!(condition_of(&job(&api), "Failed"), Some(("True".into(), "FailedIndexes".into())));
    }

    #[test]
    fn success_policy_completes_on_the_named_indexes() {
        let api = api();
        create_job(&api, json!({"completions": 3, "parallelism": 3, "completionMode": "Indexed",
            "successPolicy": {"rules": [{"succeededIndexes": "1-2"}]}}));
        reconcile_job(&api);
        let pods = active_pods(&api);
        succeed(&api, &pods[1]);
        reconcile_job(&api);
        assert!(!util::condition_true(&job(&api), "Complete"));
        succeed(&api, &pods[2]);
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Complete"), Some(("True".into(), "SuccessPolicy".into())));
        assert_eq!(condition_of(&j, "SuccessCriteriaMet"), Some(("True".into(), "SuccessPolicy".into())));
        // Index 0 was still running and is stopped.
        assert!(active_pods(&api).is_empty());
        assert_eq!(j["status"]["completedIndexes"], "1-2");
    }

    #[test]
    fn success_policy_counts() {
        let rule = |r: Value| json!({"spec": {"successPolicy": {"rules": [r]}}});
        let set = |v: &[i64]| v.iter().copied().collect::<BTreeSet<i64>>();
        assert!(success_policy_met(&rule(json!({"succeededCount": 2})), &set(&[0, 4])));
        assert!(!success_policy_met(&rule(json!({"succeededCount": 3})), &set(&[0, 4])));
        assert!(success_policy_met(&rule(json!({"succeededIndexes": "0-3", "succeededCount": 1})), &set(&[2])));
        assert!(!success_policy_met(&rule(json!({"succeededIndexes": "0-3", "succeededCount": 2})), &set(&[2, 5])));
        assert!(!success_policy_met(&rule(json!({})), &set(&[0])));
        assert!(!success_policy_met(&json!({"spec": {}}), &set(&[0])));
    }

    #[test]
    fn suspend_stops_pods_and_resume_starts_them() {
        let api = api();
        create_job(&api, json!({"completions": 2, "parallelism": 2, "suspend": true}));
        reconcile_job(&api);
        let j = job(&api);
        assert!(active_pods(&api).is_empty());
        assert_eq!(condition_of(&j, "Suspended"), Some(("True".into(), "JobSuspended".into())));
        assert!(j["status"].get("startTime").is_none());

        set_spec(&api, |s| s["suspend"] = json!(false));
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(active_pods(&api).len(), 2);
        assert_eq!(condition_of(&j, "Suspended"), Some(("False".into(), "JobResumed".into())));
        assert!(j["status"]["startTime"].is_string());

        set_spec(&api, |s| s["suspend"] = json!(true));
        reconcile_job(&api);
        assert!(active_pods(&api).is_empty());
        assert_eq!(condition_of(&job(&api), "Suspended"), Some(("True".into(), "JobSuspended".into())));
        assert_eq!(job(&api)["status"]["active"], 0);
    }

    #[test]
    fn active_deadline_fails_the_job() {
        let api = api();
        create_job(&api, json!({"activeDeadlineSeconds": 60}));
        reconcile_job(&api);
        assert_eq!(active_pods(&api).len(), 1);
        assert!(!util::condition_true(&job(&api), "Failed"));
        let j = job(&api);
        let mut st = j["status"].clone();
        st["startTime"] = json!(util::rfc3339(util::now() - chrono::Duration::seconds(61)));
        util::write_status(&api, "batch", "jobs", &j, st).unwrap();
        reconcile_job(&api);
        let j = job(&api);
        assert_eq!(condition_of(&j, "Failed"), Some(("True".into(), "DeadlineExceeded".into())));
        assert!(active_pods(&api).is_empty());
    }

    #[test]
    fn ttl_after_finished_deletes_the_job() {
        let api = api();
        create_job(&api, json!({"ttlSecondsAfterFinished": 100}));
        reconcile_job(&api);
        succeed(&api, &active_pods(&api)[0]);
        reconcile_job(&api);
        assert!(util::condition_true(&job(&api), "Complete"));
        let left = reconcile_job(&api).expect("requeued until the TTL");
        assert!(left > Duration::from_secs(95) && left <= Duration::from_secs(100), "{left:?}");
        assert!(api.get_object("batch", "jobs", "default", "pi").is_some());

        // A failed job without completionTime counts from its condition.
        let j = job(&api);
        let mut st = j["status"].clone();
        st.as_object_mut().unwrap().remove("completionTime");
        st["conditions"] = json!([{"type": "Failed", "status": "True", "lastTransitionTime": util::rfc3339(util::now() - chrono::Duration::seconds(200))}]);
        util::write_status(&api, "batch", "jobs", &j, st).unwrap();
        assert_eq!(reconcile_job(&api), None);
        assert!(api.get_object("batch", "jobs", "default", "pi").is_none());
        // Gone: nothing to do.
        assert_eq!(reconcile_job(&api), None);
    }

    #[test]
    fn ttl_zero_deletes_a_finished_job_at_once() {
        let api = api();
        create_job(&api, json!({"ttlSecondsAfterFinished": 0}));
        reconcile_job(&api);
        fail(&api, &active_pods(&api)[0], 1);
        set_spec(&api, |s| s["backoffLimit"] = json!(0));
        reconcile_job(&api);
        assert!(util::condition_true(&job(&api), "Failed"));
        assert_eq!(reconcile_job(&api), None);
        assert!(api.get_object("batch", "jobs", "default", "pi").is_none());
    }

    #[test]
    fn failed_container_reports_the_first_nonzero_exit() {
        let pod = json!({"status": {"containerStatuses": [
            {"name": "a", "state": {"terminated": {"exitCode": 0}}},
            {"name": "b", "state": {"terminated": {"exitCode": 5}}},
        ]}});
        assert_eq!(failed_container(&pod), ("b".to_string(), 5));
        assert_eq!(failed_container(&json!({})), (String::new(), 0));
    }
}
