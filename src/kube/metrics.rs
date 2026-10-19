//! `/metrics`: what the API server itself costs over time, in the
//! Prometheus text format. Request counts and latency per verb, open
//! watches, object-log size and compactions, connected node agents. The
//! soak test (bench/soak.sh) samples it to find leaks and unbounded growth.
//! Also `apiserver_request_total` and `apiserver_longrunning_requests` with
//! kube-apiserver's names and labels, so the request load a client puts on
//! litek8s compares directly with a real cluster's (bench/ argocd phases).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Method;
use hyper::body::{Body as HttpBody, Bytes, Frame, SizeHint};

use super::server::Body;

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

/// Give freed heap back to the OS now and then. glibc keeps what a burst
/// (a Helm chart's CRDs, a big list) allocated; on a 1 GiB node that is the
/// difference between ~40 MB and ~400 MB resident after an install.
pub fn spawn_memory_trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            // SAFETY: malloc_trim has no preconditions.
            let _ = tokio::task::spawn_blocking(|| unsafe { libc::malloc_trim(0) }).await;
        }
    });
}

/// This process's resident set size in bytes (Linux), else 0.
pub fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// kube-apiserver's request metrics

/// What a request does, in kube-apiserver's terms (its `RequestInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestInfo {
    pub verb: &'static str,
    pub group: String,
    pub version: String,
    pub resource: String,
    pub subresource: String,
    /// `cluster`, `namespace` or `resource` (one named object); empty for
    /// non-resource paths such as discovery.
    pub scope: &'static str,
}

impl RequestInfo {
    pub fn is_long_running(&self) -> bool {
        matches!(self.verb, "WATCH" | "CONNECT")
    }
}

/// Classifies a request like kube-apiserver does for its metrics.
pub fn request_info(method: &Method, path: &str, query: &HashMap<String, String>, content_type: &str, upgrade: bool) -> RequestInfo {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (group, version, rest) = match segs.as_slice() {
        ["api", version, rest @ ..] if !rest.is_empty() => ("", *version, rest),
        ["apis", group, version, rest @ ..] if !rest.is_empty() => (*group, *version, rest),
        _ => {
            return RequestInfo {
                verb: non_resource_verb(method),
                group: String::new(),
                version: String::new(),
                resource: String::new(),
                subresource: String::new(),
                scope: "",
            };
        }
    };
    let (legacy_watch, rest) = match rest {
        ["watch", tail @ ..] => (true, tail),
        other => (false, other),
    };
    // `namespaces/<ns>/<resource>...` is namespaced, except the namespace's
    // own subresources (`namespaces/<ns>/status`, `/finalize`).
    let (namespaced, rest) = match rest {
        ["namespaces", _, resource, tail @ ..] if !matches!(*resource, "status" | "finalize") => {
            (true, [&[*resource][..], tail].concat())
        }
        other => (false, other.to_vec()),
    };
    let resource = rest.first().copied().unwrap_or_default();
    let name = rest.get(1).copied();
    let subresource = rest.get(2).copied().unwrap_or_default();
    let watch = legacy_watch || matches!(query.get("watch").map(String::as_str), Some("true" | "1"));
    let verb = match *method {
        _ if upgrade => "CONNECT",
        Method::GET | Method::HEAD if watch => "WATCH",
        Method::GET | Method::HEAD if name.is_none() => "LIST",
        Method::GET | Method::HEAD => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH if content_type.starts_with("application/apply-patch") => "APPLY",
        Method::PATCH => "PATCH",
        Method::DELETE if name.is_none() => "DELETECOLLECTION",
        Method::DELETE => "DELETE",
        _ => non_resource_verb(method),
    };
    let scope = if name.is_some() {
        "resource"
    } else if namespaced {
        "namespace"
    } else {
        "cluster"
    };
    RequestInfo {
        verb,
        group: group.to_string(),
        version: version.to_string(),
        resource: resource.to_string(),
        subresource: subresource.to_string(),
        scope,
    }
}

fn non_resource_verb(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        _ => "OTHER",
    }
}

#[derive(Default)]
pub struct RequestMetrics {
    total: Mutex<HashMap<(RequestInfo, u16), u64>>,
    long_running: Arc<Mutex<HashMap<RequestInfo, i64>>>,
}

impl RequestMetrics {
    /// Counts a finished request; a long-running one (a watch, an exec)
    /// also counts as in flight until `body` is dropped.
    pub fn observe(&self, info: RequestInfo, code: u16, body: Body) -> Body {
        let long_running = info.is_long_running() && code < 300;
        if long_running {
            *self.long_running.lock().unwrap().entry(info.clone()).or_default() += 1;
        }
        *self.total.lock().unwrap().entry((info.clone(), code)).or_default() += 1;
        if !long_running {
            return body;
        }
        let guard = InFlight { info, gauge: self.long_running.clone() };
        http_body_util::BodyExt::boxed_unsync(Tracked { inner: body, _guard: guard })
    }

    /// The Prometheus text exposition.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP apiserver_request_total Counter of apiserver requests broken out for each verb, dry run value, group, version, resource, scope, component, and HTTP response code.\n");
        out.push_str("# TYPE apiserver_request_total counter\n");
        let total: BTreeMap<_, _> = self.total.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        for ((info, code), n) in total {
            out.push_str(&format!("apiserver_request_total{{code=\"{code}\",{}}} {n}\n", labels(&info)));
        }
        out.push_str("# HELP apiserver_longrunning_requests Gauge of all active long-running apiserver requests broken out by verb, group, version, resource, scope and component.\n");
        out.push_str("# TYPE apiserver_longrunning_requests gauge\n");
        let running: BTreeMap<_, _> = self.long_running.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (info, n) in running {
            out.push_str(&format!("apiserver_longrunning_requests{{{}}} {n}\n", labels(&info)));
        }
        out
    }
}

fn labels(info: &RequestInfo) -> String {
    format!(
        "component=\"apiserver\",dry_run=\"\",group=\"{}\",resource=\"{}\",scope=\"{}\",subresource=\"{}\",verb=\"{}\",version=\"{}\"",
        info.group, info.resource, info.scope, info.subresource, info.verb, info.version
    )
}

struct InFlight {
    info: RequestInfo,
    gauge: Arc<Mutex<HashMap<RequestInfo, i64>>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut gauge = self.gauge.lock().unwrap();
        if let Some(n) = gauge.get_mut(&self.info) {
            *n -= 1;
        }
    }
}

/// A response body that keeps its request counted as in flight.
struct Tracked {
    inner: Body,
    _guard: InFlight,
}

impl HttpBody for Tracked {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
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

#[cfg(test)]
mod request_tests {
    use super::*;

    fn info(method: Method, path: &str, query: &str) -> RequestInfo {
        let query = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        request_info(&method, path, &query, "", false)
    }

    #[test]
    fn classifies_like_kube_apiserver() {
        let i = info(Method::GET, "/api/v1/pods", "");
        assert_eq!((i.verb, i.resource.as_str(), i.scope), ("LIST", "pods", "cluster"));
        let i = info(Method::GET, "/api/v1/namespaces/default/pods", "watch=true");
        assert_eq!((i.verb, i.resource.as_str(), i.scope), ("WATCH", "pods", "namespace"));
        let i = info(Method::GET, "/api/v1/watch/namespaces/default/pods", "");
        assert_eq!((i.verb, i.scope), ("WATCH", "namespace"));
        let i = info(Method::PUT, "/apis/apps/v1/namespaces/a/deployments/web/status", "");
        assert_eq!(
            (i.verb, i.group.as_str(), i.resource.as_str(), i.subresource.as_str(), i.scope),
            ("PUT", "apps", "deployments", "status", "resource")
        );
        let i = info(Method::GET, "/api/v1/namespaces/kube-system", "");
        assert_eq!((i.verb, i.resource.as_str(), i.scope), ("GET", "namespaces", "resource"));
        let i = info(Method::PUT, "/api/v1/namespaces/a/finalize", "");
        assert_eq!((i.resource.as_str(), i.subresource.as_str()), ("namespaces", "finalize"));
        let i = info(Method::DELETE, "/api/v1/namespaces/a/pods", "");
        assert_eq!(i.verb, "DELETECOLLECTION");
        let i = info(Method::GET, "/apis/apps/v1", "");
        assert_eq!((i.verb, i.resource.as_str(), i.scope), ("GET", "", ""));
    }

    #[test]
    fn watches_are_in_flight_until_the_body_drops() {
        let m = RequestMetrics::default();
        let w = info(Method::GET, "/api/v1/pods", "watch=1");
        let body = m.observe(w, 200, crate::kube::server::full(Bytes::new()));
        assert!(m.render().contains("apiserver_longrunning_requests{component=\"apiserver\",dry_run=\"\",group=\"\",resource=\"pods\",scope=\"cluster\",subresource=\"\",verb=\"WATCH\",version=\"v1\"} 1"));
        drop(body);
        assert!(m.render().contains("verb=\"WATCH\",version=\"v1\"} 0"));
        assert!(m.render().contains("apiserver_request_total{code=\"200\",component=\"apiserver\",dry_run=\"\",group=\"\",resource=\"pods\",scope=\"cluster\",subresource=\"\",verb=\"WATCH\",version=\"v1\"} 1"));
    }
}
