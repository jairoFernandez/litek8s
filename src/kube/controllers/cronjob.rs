//! CronJob: create a Job at each scheduled time (UTC), keep history limits.

use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc};
use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const SCHEDULED_ANNOTATION: &str = "batch.kubernetes.io/cronjob-scheduled-timestamp";

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(cj) = api.get_object("batch", "cronjobs", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&cj) {
        return Ok(None);
    }
    let Some(schedule) = Schedule::parse(s(&cj, "/spec/schedule")) else {
        eprintln!("controllers: cronjob {ns}/{name}: unparseable schedule `{}`", s(&cj, "/spec/schedule"));
        return Ok(None);
    };
    let now = util::now();
    let jobs = util::owned(api, "batch", "jobs", ns, &cj);
    let is_finished = |j: &Value| util::condition_true(j, "Complete") || util::condition_true(j, "Failed");
    let mut active: Vec<&Value> = jobs.iter().filter(|j| !is_finished(j)).collect();

    // History limits, oldest first.
    for (failed, limit) in [(false, i(&cj, "/spec/successfulJobsHistoryLimit").unwrap_or(3)), (true, i(&cj, "/spec/failedJobsHistoryLimit").unwrap_or(1))] {
        let mut done: Vec<&Value> = jobs
            .iter()
            .filter(|j| is_finished(j) && util::condition_true(j, "Failed") == failed)
            .collect();
        done.sort_by(|a, b| s(a, "/metadata/creationTimestamp").cmp(s(b, "/metadata/creationTimestamp")));
        let excess = done.len().saturating_sub(limit.max(0) as usize);
        for job in &done[..excess] {
            let _ = api.delete_named("batch", "jobs", ns, &util::name(job));
        }
    }

    let mut status = cj.get("status").cloned().unwrap_or_else(|| json!({}));
    if !status.is_object() {
        status = json!({});
    }
    let last = status
        .get("lastScheduleTime")
        .and_then(Value::as_str)
        .and_then(util::parse_time)
        .or_else(|| util::parse_time(s(&cj, "/metadata/creationTimestamp")))
        .unwrap_or(now);
    let suspended = cj.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);

    if !suspended {
        let mut earliest = last;
        if let Some(deadline) = i(&cj, "/spec/startingDeadlineSeconds") {
            earliest = earliest.max(now - ChronoDuration::seconds(deadline));
        }
        // The most recent schedule time in (earliest, now], if any.
        let mut due = None;
        let mut t = earliest;
        for _ in 0..1000 {
            match schedule.next_after(t) {
                Some(next) if next <= now => {
                    due = Some(next);
                    t = next;
                }
                _ => break,
            }
        }
        if let Some(scheduled) = due {
            let policy = s(&cj, "/spec/concurrencyPolicy");
            let skip = policy == "Forbid" && !active.is_empty();
            if !skip {
                if policy == "Replace" {
                    for job in active.drain(..) {
                        let _ = api.delete_named("batch", "jobs", ns, &util::name(job));
                    }
                }
                let job_name = format!("{name}-{}", scheduled.timestamp() / 60);
                if !jobs.iter().any(|j| util::name(j) == job_name) {
                    let created = api.create_object("batch", "jobs", Some(ns), job_for(&cj, &job_name, scheduled))?;
                    active_refs_push(&mut status, &created);
                }
                status["lastScheduleTime"] = json!(util::rfc3339(scheduled));
            }
        }
    }

    let refs: Vec<Value> = active
        .iter()
        .map(|j| json!({"apiVersion": "batch/v1", "kind": "Job", "name": util::name(j), "namespace": ns, "uid": util::uid(j)}))
        .collect();
    let mut merged = refs;
    if let Some(Value::Array(created)) = status.get("active") {
        for r in created {
            if !merged.iter().any(|m| m["uid"] == r["uid"]) && jobs.iter().all(|j| util::uid(j) != r["uid"]) {
                merged.push(r.clone());
            }
        }
    }
    if merged.is_empty() {
        if let Some(m) = status.as_object_mut() {
            m.remove("active");
        }
    } else {
        status["active"] = json!(merged);
    }
    let last_success = jobs
        .iter()
        .filter(|j| util::condition_true(j, "Complete"))
        .filter_map(|j| j.pointer("/status/completionTime").and_then(Value::as_str))
        .max()
        .map(str::to_string);
    if let Some(t) = last_success {
        status["lastSuccessfulTime"] = json!(t);
    }
    util::write_status(api, "batch", "cronjobs", &cj, status)?;

    // Wake up for the next schedule time.
    let next = schedule.next_after(now.max(last)).unwrap_or(now + ChronoDuration::hours(1));
    let wait = (next - util::now()).num_milliseconds().max(0) as u64 + 200;
    Ok(Some(Duration::from_millis(wait)))
}

fn active_refs_push(status: &mut Value, job: &Value) {
    if !status.get("active").is_some_and(Value::is_array) {
        status["active"] = json!([]);
    }
    status["active"].as_array_mut().unwrap().push(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "name": util::name(job),
        "namespace": s(job, "/metadata/namespace"),
        "uid": util::uid(job),
    }));
}

fn job_for(cj: &Value, name: &str, scheduled: DateTime<Utc>) -> Value {
    let template = cj.pointer("/spec/jobTemplate").cloned().unwrap_or_else(|| json!({}));
    let mut meta = template.get("metadata").cloned().unwrap_or_else(|| json!({}));
    if !meta.is_object() {
        meta = json!({});
    }
    meta["name"] = json!(name);
    if !meta.get("annotations").is_some_and(Value::is_object) {
        meta["annotations"] = json!({});
    }
    meta["annotations"][SCHEDULED_ANNOTATION] = json!(util::rfc3339(scheduled));
    meta["ownerReferences"] = json!([util::owner_ref(cj)]);
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": meta,
        "spec": template.get("spec").cloned().unwrap_or_else(|| json!({})),
    })
}

/// A standard 5-field cron schedule (minute hour day-of-month month
/// day-of-week), plus the `@hourly`-style macros. Evaluated in UTC.
#[derive(Debug, Clone)]
pub(crate) struct Schedule {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    days_restricted: bool,
    weekdays_restricted: bool,
}

impl Schedule {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        // A `CRON_TZ=`/`TZ=` prefix is accepted; times stay UTC.
        let raw = match raw.split_once(' ') {
            Some((tz, rest)) if tz.starts_with("TZ=") || tz.starts_with("CRON_TZ=") => rest.trim(),
            _ => raw,
        };
        let expanded = match raw {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return None;
        }
        const MONTHS: &[&str] = &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        const DAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let mut weekdays = parse_field(fields[4], 0, 7, DAYS, 0)?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        weekdays.truncate(7);
        Some(Self {
            minutes: parse_field(fields[0], 0, 59, &[], 0)?,
            hours: parse_field(fields[1], 0, 23, &[], 0)?,
            days: parse_field(fields[2], 1, 31, &[], 0)?,
            months: parse_field(fields[3], 1, 12, MONTHS, 1)?,
            weekdays,
            days_restricted: !fields[2].starts_with('*'),
            weekdays_restricted: !fields[4].starts_with('*'),
        })
    }

    fn day_matches(&self, t: &DateTime<Utc>) -> bool {
        let dom = self.days[t.day() as usize];
        let dow = self.weekdays[t.weekday().num_days_from_sunday() as usize];
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// The first matching minute strictly after `after`.
    pub(crate) fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut t = after.with_second(0)?.with_nanosecond(0)? + ChronoDuration::minutes(1);
        let limit = after + ChronoDuration::days(366 * 5);
        while t <= limit {
            if !self.months[t.month() as usize] {
                // Jump to the first day of the next month.
                let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
                t = t.with_day(1)?.with_hour(0)?.with_minute(0)?.with_month(m)?.with_year(y)?;
                continue;
            }
            if !self.day_matches(&t) {
                t = (t + ChronoDuration::days(1)).with_hour(0)?.with_minute(0)?;
                continue;
            }
            if !self.hours[t.hour() as usize] {
                t = (t + ChronoDuration::hours(1)).with_minute(0)?;
                continue;
            }
            if !self.minutes[t.minute() as usize] {
                t += ChronoDuration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }
}

/// One cron field into a membership table indexed by value (0..=max).
fn parse_field(field: &str, min: u32, max: u32, names: &[&str], name_base: u32) -> Option<Vec<bool>> {
    let mut set = vec![false; max as usize + 1];
    let value = |v: &str| -> Option<u32> {
        if let Ok(n) = v.parse::<u32>() {
            return Some(n);
        }
        let lower = v.to_ascii_lowercase();
        names.iter().position(|n| *n == lower).map(|p| p as u32 + name_base)
    };
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            (v, if part.contains('/') { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            return None;
        }
        let mut v = lo;
        while v <= hi {
            set[v as usize] = true;
            v += step;
        }
    }
    Some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn every_minute_and_steps() {
        let s = Schedule::parse("*/1 * * * *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), Some(at(2026, 1, 1, 0, 1)));
        let s = Schedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 1)), Some(at(2026, 1, 1, 0, 15)));
    }

    #[test]
    fn fields_and_macros() {
        let s = Schedule::parse("30 2 * * mon-fri").unwrap();
        // 2026-01-03 is a Saturday.
        assert_eq!(s.next_after(at(2026, 1, 3, 0, 0)), Some(at(2026, 1, 5, 2, 30)));
        let s = Schedule::parse("@monthly").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 15, 0, 0)), Some(at(2026, 2, 1, 0, 0)));
        assert!(Schedule::parse("61 * * * *").is_none());
        assert!(Schedule::parse("* * *").is_none());
    }
}
