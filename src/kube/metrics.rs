//! `/metrics`: what the API server itself costs over time, in the
//! Prometheus text format. Request counts and latency per verb, open
//! watches, object-log size and compactions, connected node agents. The
//! soak test (bench/soak.sh) samples it to find leaks and unbounded growth.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

const VERBS: [&str; 8] = ["get", "list", "watch", "create", "update", "patch", "delete", "other"];
/// Upper bounds of the latency buckets, in seconds.
const BUCKETS: [f64; 10] = [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 5.0];

#[derive(Default)]
struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    /// Microseconds.
    sum: AtomicU64,
}

#[derive(Default)]
pub struct Metrics {
    requests: [Histogram; VERBS.len()],
    watches: Arc<AtomicI64>,
}

/// Counts one open watch while alive.
pub struct WatchGuard(Arc<AtomicI64>);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    /// The verb class of a request, as Kubernetes names them.
    pub fn verb(method: &hyper::Method, watch: bool, named: bool) -> &'static str {
        match *method {
            _ if watch => "watch",
            hyper::Method::GET if named => "get",
            hyper::Method::GET => "list",
            hyper::Method::POST => "create",
            hyper::Method::PUT => "update",
            hyper::Method::PATCH => "patch",
            hyper::Method::DELETE => "delete",
            _ => "other",
        }
    }

    /// Record a finished request (watches are counted when they end).
    pub fn observe(&self, verb: &str, elapsed: Duration) {
        let i = VERBS.iter().position(|v| *v == verb).unwrap_or(VERBS.len() - 1);
        let h = &self.requests[i];
        let secs = elapsed.as_secs_f64();
        for (b, bound) in h.buckets.iter().zip(BUCKETS) {
            if secs <= bound {
                b.fetch_add(1, Ordering::Relaxed);
            }
        }
        h.count.fetch_add(1, Ordering::Relaxed);
        h.sum.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn watch_started(&self) -> WatchGuard {
        self.watches.fetch_add(1, Ordering::Relaxed);
        WatchGuard(self.watches.clone())
    }

    pub fn open_watches(&self) -> i64 {
        self.watches.load(Ordering::Relaxed)
    }

    pub fn render(&self, out: &mut String) {
        out.push_str("# TYPE litek8s_apiserver_request_duration_seconds histogram\n");
        for (verb, h) in VERBS.iter().zip(&self.requests) {
            let count = h.count.load(Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            for (b, bound) in h.buckets.iter().zip(BUCKETS) {
                let _ = writeln!(out, "litek8s_apiserver_request_duration_seconds_bucket{{verb=\"{verb}\",le=\"{bound}\"}} {}", b.load(Ordering::Relaxed));
            }
            let _ = writeln!(out, "litek8s_apiserver_request_duration_seconds_bucket{{verb=\"{verb}\",le=\"+Inf\"}} {count}");
            let _ = writeln!(out, "litek8s_apiserver_request_duration_seconds_sum{{verb=\"{verb}\"}} {}", h.sum.load(Ordering::Relaxed) as f64 / 1e6);
            let _ = writeln!(out, "litek8s_apiserver_request_duration_seconds_count{{verb=\"{verb}\"}} {count}");
        }
        let _ = writeln!(out, "# TYPE litek8s_apiserver_watches gauge\nlitek8s_apiserver_watches {}", self.open_watches());
    }
}

/// A gauge line.
pub fn gauge(out: &mut String, name: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "# TYPE {name} gauge\n{name} {value}");
}

/// This process's resident set size in bytes (Linux), else 0.
pub fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_histograms_and_watches() {
        let m = Metrics::default();
        m.observe("get", Duration::from_millis(3));
        m.observe("get", Duration::from_millis(30));
        let guard = m.watch_started();
        let mut out = String::new();
        m.render(&mut out);
        assert!(out.contains("litek8s_apiserver_request_duration_seconds_bucket{verb=\"get\",le=\"0.005\"} 1\n"));
        assert!(out.contains("litek8s_apiserver_request_duration_seconds_bucket{verb=\"get\",le=\"0.05\"} 2\n"));
        assert!(out.contains("litek8s_apiserver_request_duration_seconds_count{verb=\"get\"} 2\n"));
        assert!(!out.contains("verb=\"list\""));
        assert!(out.contains("litek8s_apiserver_watches 1\n"));
        drop(guard);
        assert_eq!(m.open_watches(), 0);
    }
}
