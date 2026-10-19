//! The API server's request metrics, served at `/metrics` in the Prometheus
//! text format with the same names and labels as kube-apiserver
//! (`apiserver_request_total`, `apiserver_longrunning_requests`), so the
//! request load a client puts on litek8s can be compared with a real cluster.

use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use hyper::Method;
use hyper::body::{Body as HttpBody, Bytes, Frame, SizeHint};

use super::server::Body;

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
