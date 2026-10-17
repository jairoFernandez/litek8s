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

    let pods = util::owned(api, "", "pods", ns, &job);
    let succeeded_pods: Vec<&Value> = pods.iter().filter(|p| util::phase(p) == "Succeeded").collect();
    let failed = pods.iter().filter(|p| util::phase(p) == "Failed").count() as i64;
    let active: Vec<&Value> = pods.iter().filter(|p| util::is_active(p)).collect();
    let indexed = s(&job, "/spec/completionMode") == "Indexed";
    let completions = i(&job, "/spec/completions");
    let parallelism = i(&job, "/spec/parallelism").unwrap_or(1).max(0);
    let backoff_limit = i(&job, "/spec/backoffLimit").unwrap_or(6);
    let suspended = job.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);

    let succeeded_indexes: BTreeSet<i64> = succeeded_pods.iter().filter_map(|p| pod_index(p)).collect();
    let succeeded = if indexed { succeeded_indexes.len() as i64 } else { succeeded_pods.len() as i64 };

    let mut status = job.get("status").cloned().unwrap_or_else(|| json!({}));
    if !status.is_object() {
        status = json!({});
    }
    if status.get("startTime").is_none() && !suspended {
        status["startTime"] = json!(util::rfc3339(now));
    }
    let mut conditions = util::conditions_of(&job);

    let complete = match completions {
        Some(c) => succeeded >= c,
        None => succeeded >= 1 && active.is_empty(),
    };
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
        util::set_condition(&mut conditions, "SuccessCriteriaMet", "True", "CompletionsReached", "Reached expected number of succeeded pods");
        util::set_condition(&mut conditions, "Complete", "True", "CompletionsReached", "Reached expected number of succeeded pods");
        status["completionTime"] = json!(util::rfc3339(now));
        status["active"] = json!(0);
        status["ready"] = json!(0);
    } else if failed > backoff_limit || deadline_exceeded {
        for pod in &active {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
        let (reason, message) = if deadline_exceeded {
            ("DeadlineExceeded", "Job was active longer than specified deadline")
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
        let backoff = if failed > 0 {
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
            let remaining = completions.map(|c| c - succeeded).unwrap_or(parallelism);
            let want = parallelism.min(remaining).max(0);
            let missing = want - active.len() as i64;
            if missing > 0 {
                let template = job.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
                let mut free_indexes = (0..completions.unwrap_or(0))
                    .filter(|idx| !succeeded_indexes.contains(idx) && !active.iter().any(|p| pod_index(p) == Some(*idx)));
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
    }
}
