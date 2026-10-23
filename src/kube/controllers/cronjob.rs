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
        if let Some(scheduled) = schedule.most_recent(earliest, now) {
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
    /// The most recent schedule time in (earliest, now], if any. Searched
    /// back from `now` in doubling windows, so a CronJob that missed many
    /// runs starts the latest one, as Kubernetes does, not one from where a
    /// forward walk would give up.
    pub(crate) fn most_recent(&self, earliest: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut window = ChronoDuration::minutes(1);
        loop {
            let from = earliest.max(now - window);
            let mut due = None;
            let mut t = from;
            for _ in 0..1000 {
                match self.next_after(t) {
                    Some(next) if next <= now => {
                        due = Some(next);
                        t = next;
                    }
                    _ => break,
                }
            }
            if due.is_some() || from == earliest {
                return due;
            }
            window = window * 2;
        }
    }

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

    #[test]
    fn lists_ranges_steps_and_names() {
        let s = Schedule::parse("5,10-20/5 * * * *").unwrap();
        let minutes: Vec<u32> = (0..60).filter(|m| s.minutes[*m as usize]).collect();
        assert_eq!(minutes, [5, 10, 15, 20]);
        // `a/n` runs from a to the end of the range.
        let s = Schedule::parse("50/5 * * * *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 56)), Some(at(2026, 1, 1, 1, 50)));
        assert!(Schedule::parse("? * * * *").unwrap().minutes.iter().all(|m| *m));
        let s = Schedule::parse("0 0 1 JAN,jul *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), Some(at(2026, 7, 1, 0, 0)));
        assert_eq!(s.next_after(at(2026, 7, 1, 0, 0)), Some(at(2027, 1, 1, 0, 0)));
        assert!(Schedule::parse("*/0 * * * *").is_none());
        assert!(Schedule::parse("10-5 * * * *").is_none());
        assert!(Schedule::parse("* * 0 * *").is_none());
        assert!(Schedule::parse("* * * 13 *").is_none());
        assert!(Schedule::parse("* * * * fun").is_none());
        assert!(Schedule::parse("* 24 * * *").is_none());
    }

    #[test]
    fn sunday_is_zero_or_seven() {
        // 2026-01-04 is a Sunday.
        for raw in ["0 12 * * 0", "0 12 * * 7", "0 12 * * SUN", "@weekly"] {
            let s = Schedule::parse(raw).unwrap();
            let want = if raw == "@weekly" { at(2026, 1, 4, 0, 0) } else { at(2026, 1, 4, 12, 0) };
            assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), Some(want), "{raw}");
        }
    }

    #[test]
    fn day_of_month_or_day_of_week_when_both_restricted() {
        // The 15th or any Friday; 2026-01-02 is a Friday.
        let s = Schedule::parse("0 0 15 * fri").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), Some(at(2026, 1, 2, 0, 0)));
        assert_eq!(s.next_after(at(2026, 1, 9, 0, 0)), Some(at(2026, 1, 15, 0, 0)));
        // Only the day of month.
        let s = Schedule::parse("0 0 15 * *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), Some(at(2026, 1, 15, 0, 0)));
    }

    #[test]
    fn macros_and_time_zone_prefix() {
        let from = at(2026, 3, 10, 7, 30);
        for (raw, want) in [
            ("@yearly", at(2027, 1, 1, 0, 0)),
            ("@annually", at(2027, 1, 1, 0, 0)),
            ("@daily", at(2026, 3, 11, 0, 0)),
            ("@midnight", at(2026, 3, 11, 0, 0)),
            ("@hourly", at(2026, 3, 10, 8, 0)),
            ("CRON_TZ=Europe/Madrid 0 9 * * *", at(2026, 3, 10, 9, 0)),
            ("TZ=UTC  @hourly", at(2026, 3, 10, 8, 0)),
        ] {
            assert_eq!(Schedule::parse(raw).unwrap().next_after(from), Some(want), "{raw}");
        }
    }

    #[test]
    fn next_after_skips_months_and_gives_up_on_impossible_dates() {
        let s = Schedule::parse("0 0 29 2 *").unwrap();
        assert_eq!(s.next_after(at(2026, 3, 1, 0, 0)), Some(at(2028, 2, 29, 0, 0)));
        let s = Schedule::parse("0 0 31 2 *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), None);
        let s = Schedule::parse("59 23 31 12 *").unwrap();
        assert_eq!(s.next_after(at(2026, 12, 31, 23, 59)), Some(at(2027, 12, 31, 23, 59)));
        // Seconds are dropped: strictly after the minute.
        let s = Schedule::parse("* * * * *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0) + ChronoDuration::seconds(59)), Some(at(2026, 1, 1, 0, 1)));
    }

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn create_cj(api: &ApiState, spec: Value) -> Value {
        let mut spec = spec;
        spec["jobTemplate"] = json!({
            "metadata": {"labels": {"app": "tick"}, "annotations": {"note": "x"}},
            "spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [{"name": "c", "image": "busybox"}]}}},
        });
        let cj = json!({"apiVersion": "batch/v1", "kind": "CronJob", "metadata": {"name": "tick"}, "spec": spec});
        api.create_object("batch", "cronjobs", Some("default"), cj).unwrap()
    }

    fn cj(api: &ApiState) -> Value {
        api.get_object("batch", "cronjobs", "default", "tick").unwrap()
    }

    fn set_last_schedule(api: &ApiState, t: DateTime<Utc>) {
        let c = cj(api);
        let mut st = c.get("status").cloned().unwrap_or_else(|| json!({}));
        st["lastScheduleTime"] = json!(util::rfc3339(t));
        util::write_status(api, "batch", "cronjobs", &c, st).unwrap();
    }

    fn set_spec(api: &ApiState, f: impl FnOnce(&mut Value)) {
        let mut c = cj(api);
        f(&mut c["spec"]);
        api.update_object("batch", "cronjobs", c).unwrap();
    }

    fn jobs(api: &ApiState) -> Vec<Value> {
        api.store().list("batch", "jobs", Some("default")).0.into_iter().filter(|j| !util::is_deleting(j)).collect()
    }

    fn job_names(api: &ApiState) -> Vec<String> {
        let mut names: Vec<String> = jobs(api).iter().map(util::name).collect();
        names.sort();
        names
    }

    /// The most recent minute boundary at or before now.
    fn this_minute() -> DateTime<Utc> {
        // Keep clear of the next minute, so the reconcile sees the same one.
        if util::now().second() >= 58 {
            std::thread::sleep(Duration::from_secs(3));
        }
        let now = util::now();
        now.with_second(0).unwrap().with_nanosecond(0).unwrap()
    }

    fn job_name_at(t: DateTime<Utc>) -> String {
        format!("tick-{}", t.timestamp() / 60)
    }

    fn finish_job(api: &ApiState, name: &str, type_: &str, completion: Option<&str>) {
        let j = api.get_object("batch", "jobs", "default", name).unwrap();
        let mut st = json!({"conditions": [{"type": type_, "status": "True"}]});
        if let Some(t) = completion {
            st["completionTime"] = json!(t);
        }
        util::write_status(api, "batch", "jobs", &j, st).unwrap();
    }

    fn reconcile_cj(api: &ApiState) -> Requeue {
        reconcile(api, "default", "tick").unwrap()
    }

    #[test]
    fn nothing_is_due_right_after_creation() {
        let api = api();
        create_cj(&api, json!({"schedule": "0 0 1 1 *"}));
        let wait = reconcile_cj(&api).expect("wakes for the next run");
        assert!(wait > Duration::from_secs(60));
        assert!(jobs(&api).is_empty());
        assert!(cj(&api)["status"].get("lastScheduleTime").is_none());
    }

    #[test]
    fn creates_one_job_for_the_most_recent_missed_time() {
        let api = api();
        let created = create_cj(&api, json!({"schedule": "* * * * *"}));
        let minute = this_minute();
        set_last_schedule(&api, minute - ChronoDuration::minutes(10));
        let wait = reconcile_cj(&api).expect("wakes for the next minute");
        assert!(wait <= Duration::from_millis(60_200), "{wait:?}");
        assert_eq!(job_names(&api), [job_name_at(minute)]);
        let job = &jobs(&api)[0];
        assert!(util::controlled_by(job, &util::uid(&created)));
        assert_eq!(s(job, "/metadata/labels/app"), "tick");
        assert_eq!(s(job, "/metadata/annotations/note"), "x");
        assert_eq!(s(job, "/metadata/annotations/batch.kubernetes.io~1cronjob-scheduled-timestamp"), util::rfc3339(minute));
        assert_eq!(s(job, "/spec/template/spec/containers/0/image"), "busybox");
        let st = cj(&api)["status"].clone();
        assert_eq!(st["lastScheduleTime"], util::rfc3339(minute));
        assert_eq!(st["active"].as_array().unwrap().len(), 1);
        assert_eq!(st["active"][0]["name"], job_name_at(minute));
        assert_eq!(st["active"][0]["uid"], util::uid(job));

        // Same minute again: no second job, the active ref is kept once.
        reconcile_cj(&api);
        assert_eq!(jobs(&api).len(), 1);
        assert_eq!(cj(&api)["status"]["active"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn suspended_cronjob_creates_nothing() {
        let api = api();
        create_cj(&api, json!({"schedule": "* * * * *", "suspend": true}));
        set_last_schedule(&api, this_minute() - ChronoDuration::minutes(5));
        assert!(reconcile_cj(&api).is_some());
        assert!(jobs(&api).is_empty());
        set_spec(&api, |s| s["suspend"] = json!(false));
        reconcile_cj(&api);
        assert_eq!(jobs(&api).len(), 1);
    }

    #[test]
    fn starting_deadline_skips_runs_missed_too_long_ago() {
        let api = api();
        create_cj(&api, json!({"schedule": "0 0 1 1 *", "startingDeadlineSeconds": 60}));
        set_last_schedule(&api, util::now() - ChronoDuration::days(800));
        reconcile_cj(&api);
        assert!(jobs(&api).is_empty());
        // Without the deadline the last 1 January runs.
        set_spec(&api, |s| {
            s.as_object_mut().unwrap().remove("startingDeadlineSeconds");
        });
        reconcile_cj(&api);
        let year = util::now().year();
        let new_year = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(job_names(&api), [job_name_at(new_year)]);
    }

    #[test]
    fn allow_runs_jobs_concurrently() {
        let api = api();
        create_cj(&api, json!({"schedule": "* * * * *"}));
        let minute = this_minute();
        set_last_schedule(&api, minute - ChronoDuration::minutes(2));
        api.create_object("batch", "jobs", Some("default"), job_for(&cj(&api), "tick-old", minute)).unwrap();
        reconcile_cj(&api);
        assert_eq!(job_names(&api), [job_name_at(minute), "tick-old".to_string()]);
        let active: Vec<String> = cj(&api)["status"]["active"].as_array().unwrap().iter().map(|r| s(r, "/name").to_string()).collect();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"tick-old".to_string()));
    }

    #[test]
    fn forbid_skips_while_a_job_runs() {
        let api = api();
        create_cj(&api, json!({"schedule": "* * * * *", "concurrencyPolicy": "Forbid"}));
        let minute = this_minute();
        let last = minute - ChronoDuration::minutes(2);
        set_last_schedule(&api, last);
        api.create_object("batch", "jobs", Some("default"), job_for(&cj(&api), "tick-old", last)).unwrap();
        reconcile_cj(&api);
        assert_eq!(job_names(&api), ["tick-old"]);
        // The skipped run is not recorded as scheduled.
        let st = cj(&api)["status"].clone();
        assert_eq!(st["lastScheduleTime"], util::rfc3339(last));
        assert_eq!(st["active"][0]["name"], "tick-old");

        finish_job(&api, "tick-old", "Complete", Some(&util::rfc3339(minute)));
        reconcile_cj(&api);
        assert_eq!(job_names(&api), [job_name_at(minute), "tick-old".to_string()]);
        let st = cj(&api)["status"].clone();
        assert_eq!(st["lastSuccessfulTime"], util::rfc3339(minute));
        assert_eq!(st["active"].as_array().unwrap().len(), 1);
        assert_eq!(st["active"][0]["name"], job_name_at(minute));
    }

    #[test]
    fn replace_deletes_the_running_job() {
        let api = api();
        create_cj(&api, json!({"schedule": "* * * * *", "concurrencyPolicy": "Replace"}));
        let minute = this_minute();
        set_last_schedule(&api, minute - ChronoDuration::minutes(2));
        api.create_object("batch", "jobs", Some("default"), job_for(&cj(&api), "tick-old", minute)).unwrap();
        reconcile_cj(&api);
        assert_eq!(job_names(&api), [job_name_at(minute)]);
        let st = cj(&api)["status"].clone();
        assert_eq!(st["active"].as_array().unwrap().len(), 1);
        assert_eq!(st["active"][0]["name"], job_name_at(minute));
    }

    #[test]
    fn history_limits_remove_the_oldest_finished_jobs() {
        let api = api();
        create_cj(&api, json!({"schedule": "0 0 1 1 *", "successfulJobsHistoryLimit": 2, "failedJobsHistoryLimit": 0}));
        let c = cj(&api);
        let t = util::now();
        for n in 1..=3 {
            let name = format!("tick-ok-{n}");
            let mut job = job_for(&c, &name, t);
            // Distinct, ordered creation times.
            job["metadata"]["creationTimestamp"] = json!(util::rfc3339(t - ChronoDuration::minutes(10 - n)));
            api.create_object("batch", "jobs", Some("default"), job).unwrap();
            finish_job(&api, &name, "Complete", Some(&format!("2026-01-0{n}T00:00:00Z")));
        }
        api.create_object("batch", "jobs", Some("default"), job_for(&c, "tick-bad", t)).unwrap();
        finish_job(&api, "tick-bad", "Failed", None);
        api.create_object("batch", "jobs", Some("default"), job_for(&c, "tick-running", t)).unwrap();
        reconcile_cj(&api);
        assert_eq!(job_names(&api), ["tick-ok-2", "tick-ok-3", "tick-running"]);
        let st = cj(&api)["status"].clone();
        assert_eq!(st["lastSuccessfulTime"], "2026-01-03T00:00:00Z");
        assert_eq!(st["active"].as_array().unwrap().len(), 1);
        assert_eq!(st["active"][0]["name"], "tick-running");
    }

    #[test]
    fn unparseable_schedule_or_missing_cronjob_does_nothing() {
        let api = api();
        assert_eq!(reconcile_cj(&api), None);
        let Ok(_) = api.create_object(
            "batch",
            "cronjobs",
            Some("default"),
            json!({"apiVersion": "batch/v1", "kind": "CronJob", "metadata": {"name": "tick"},
                "spec": {"schedule": "every day", "jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "c", "image": "x"}]}}}}}}),
        ) else {
            return;
        };
        assert_eq!(reconcile_cj(&api), None);
        assert!(cj(&api).get("status").is_none_or(|s| s.get("lastScheduleTime").is_none()));
    }

    #[test]
    fn most_recent_is_the_latest_time_however_many_were_missed() {
        let every_minute = Schedule::parse("* * * * *").unwrap();
        let now = at(2026, 10, 23, 12, 0) + ChronoDuration::seconds(30);
        assert_eq!(every_minute.most_recent(now - ChronoDuration::minutes(2000), now), Some(at(2026, 10, 23, 12, 0)));
        assert_eq!(every_minute.most_recent(at(2026, 10, 23, 12, 0), now), None);
        let yearly = Schedule::parse("0 0 1 1 *").unwrap();
        assert_eq!(yearly.most_recent(at(2020, 6, 1, 0, 0), now), Some(at(2026, 1, 1, 0, 0)));
        assert_eq!(yearly.most_recent(at(2026, 2, 1, 0, 0), now), None);
    }

    #[test]
    fn thousands_of_missed_runs_start_the_latest() {
        let api = api();
        create_cj(&api, json!({"schedule": "* * * * *"}));
        let minute = this_minute();
        set_last_schedule(&api, minute - ChronoDuration::minutes(2000));
        reconcile_cj(&api);
        assert_eq!(job_names(&api), [job_name_at(minute)]);
        assert_eq!(cj(&api)["status"]["lastScheduleTime"], util::rfc3339(minute));
    }
}
