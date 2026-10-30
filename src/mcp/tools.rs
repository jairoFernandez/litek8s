//! The MCP tools: their schemas and handlers. Each handler answers with a
//! JSON value; [`Server::call`] wraps it as an MCP result (text and
//! `structuredContent`), cut to `--max-output`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{McpArgs, diagnose, guard};
use crate::ui::client::{Endpoint, Lines, enc};
use crate::ui::kube_gitops as gitops;
use crate::ui::kube_ops as ops;
use crate::ui::kube_resources::{Catalog, Resource};
use crate::ui::kube_state::{ObjRef, Propagation};

const FIELD_MANAGER: &str = "litek8s-mcp";
const TABLE_ACCEPT: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

/// What a tool is: name, description, input schema, whether it changes
/// anything, and how to narrow a result that was too long.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub mutating: bool,
    pub destructive: bool,
    pub narrow: &'static str,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn ctx_prop() -> Value {
    json!({"type": "string", "description": "kubeconfig context of a litek8s cluster (litek8s-<cluster name>); default: the server's --context or the current one"})
}

fn confirm_prop() -> Value {
    json!({"type": "boolean", "description": "Must be true to actually do it; without it the tool only reports what it would do"})
}

fn cluster_prop() -> Value {
    json!({"type": "string", "description": "dev cluster name (default dev)"})
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "clusters_list",
        description: "List the local litek8s dev clusters (name, running, API server, nodes, memory, kube context, Docker socket) and the shared VM/sandbox. Read-only.",
        schema: || obj(json!({}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "cluster_status",
        description: "Status of one dev cluster: running or not, server URL, kube context, Docker socket, nodes Ready, pods, memory, data directory. Read-only.",
        schema: || obj(json!({"name": cluster_prop()}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "cluster_up",
        description: "Start (or create) a dev cluster and wait until its nodes are Ready. Adds context litek8s-<name> to the kubeconfig without switching the current context unless switch_context is true. Can take a minute or two the first time.",
        schema: || {
            obj(
                json!({
                    "name": cluster_prop(),
                    "nodes": {"type": "integer", "minimum": 1, "maximum": 10, "description": "control plane + N-1 workers (remembered)"},
                    "node": {"type": "string", "description": "start only this stopped worker node of a running cluster"},
                    "switch_context": {"type": "boolean", "description": "also make it the current kubectl context (default false)"},
                    "timeout_seconds": {"type": "integer", "minimum": 10, "maximum": 900, "description": "wait for Ready nodes (default 180)"},
                }),
                &[],
            )
        },
        mutating: true,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "cluster_down",
        description: "Stop a dev cluster (objects and images are kept for the next cluster_up), or with node=<worker> stop just that worker (simulates a node failure). Destructive for running workloads: needs confirm: true.",
        schema: || obj(json!({"name": cluster_prop(), "node": {"type": "string", "description": "stop only this worker node"}, "confirm": confirm_prop()}), &["name"]),
        mutating: true,
        destructive: true,
        narrow: "",
    },
    Tool {
        name: "cluster_delete",
        description: "Delete a dev cluster: stops it and removes its objects, images, volumes and kube context. Irreversible: needs confirm: true.",
        schema: || obj(json!({"name": cluster_prop(), "confirm": confirm_prop()}), &["name"]),
        mutating: true,
        destructive: true,
        narrow: "",
    },
    Tool {
        name: "contexts_list",
        description: "The kubeconfig's contexts with their servers; probe: true also asks each one's /version (anonymously) to tell which are litek8s. Read-only.",
        schema: || obj(json!({"probe": {"type": "boolean", "description": "check which servers are litek8s (default false)"}}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "get",
        description: "Get Kubernetes resources from a litek8s cluster. Without name: a table like kubectl get (columns from the server). With name: the object (managedFields left out), its recent events and its owner chain (Pod -> ReplicaSet -> Deployment). kind accepts plural, singular, short names and kind (pods, pod, po, Deployment, deploy, crontabs.example.com). Read-only.",
        schema: || {
            obj(
                json!({
                    "kind": {"type": "string", "description": "resource type: pods, deployments, svc, nodes, events, ..."},
                    "name": {"type": "string"},
                    "namespace": {"type": "string", "description": "default: all namespaces for a list, default for one object"},
                    "label_selector": {"type": "string", "description": "e.g. app=web,tier!=db"},
                    "field_selector": {"type": "string", "description": "e.g. status.phase=Running, spec.nodeName=n1"},
                    "limit": {"type": "integer", "minimum": 1, "description": "most items (default 200)"},
                    "full": {"type": "boolean", "description": "a list returns full objects instead of the table"},
                    "context": ctx_prop(),
                }),
                &["kind"],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Narrow with namespace, label_selector, field_selector or limit, or get one object by name.",
    },
    Tool {
        name: "describe",
        description: "kubectl describe for one object: its fields in readable form, related objects (a Deployment's ReplicaSets, a Node's pods, a Service's endpoints), events, plus its owner chain. Read-only.",
        schema: || obj(json!({"kind": {"type": "string"}, "name": {"type": "string"}, "namespace": {"type": "string", "description": "default: default"}, "context": ctx_prop()}), &["kind", "name"]),
        mutating: false,
        destructive: false,
        narrow: "Use get with a name for the raw object instead.",
    },
    Tool {
        name: "events",
        description: "Recent events, newest last: of a namespace (or all), optionally of one object, optionally warnings only. Read-only.",
        schema: || {
            obj(
                json!({
                    "namespace": {"type": "string", "description": "default: all namespaces"},
                    "kind": {"type": "string", "description": "involved object's kind (Pod, Deployment, ...)"},
                    "name": {"type": "string", "description": "involved object's name"},
                    "warnings_only": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1, "description": "newest N (default 50)"},
                    "context": ctx_prop(),
                }),
                &[],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Narrow with namespace, kind/name, warnings_only or limit.",
    },
    Tool {
        name: "diagnose_pod",
        description: "Why is this pod not Ready / crash-looping / pending? One result with findings (cause, evidence, next step), phase and conditions, every container's state, last exit code and probes, the node's readiness, events, the owner chain and the last log lines (and those of the run before the last restart). Give pod, or selector to diagnose the not-ready pods it matches (up to 3). Read-only.",
        schema: || {
            obj(
                json!({
                    "namespace": {"type": "string", "description": "default: default"},
                    "pod": {"type": "string"},
                    "selector": {"type": "string", "description": "label selector instead of pod, e.g. app=web"},
                    "log_lines": {"type": "integer", "minimum": 0, "maximum": 500, "description": "last log lines per container (default 30)"},
                    "context": ctx_prop(),
                }),
                &[],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Lower log_lines, or diagnose one pod by name.",
    },
    Tool {
        name: "apply",
        description: "Apply Kubernetes manifests (YAML or JSON, several documents allowed) with server-side apply, like kubectl apply --server-side. Give manifest text or a file path. Reports created / configured / unchanged per object. dry_run: true validates without saving.",
        schema: || {
            obj(
                json!({
                    "manifest": {"type": "string", "description": "YAML or JSON text"},
                    "path": {"type": "string", "description": "a manifest file (relative to the MCP server's working directory)"},
                    "namespace": {"type": "string", "description": "for namespaced objects that name none (default: default)"},
                    "dry_run": {"type": "boolean"},
                    "context": ctx_prop(),
                }),
                &[],
            )
        },
        mutating: true,
        destructive: false,
        narrow: "Apply fewer documents at once.",
    },
    Tool {
        name: "delete",
        description: "Delete one object (children go with it, propagation Background). Destructive: needs confirm: true; without it reports what would be deleted.",
        schema: || {
            obj(
                json!({
                    "kind": {"type": "string"},
                    "name": {"type": "string"},
                    "namespace": {"type": "string", "description": "default: default"},
                    "force": {"type": "boolean", "description": "grace period 0"},
                    "confirm": confirm_prop(),
                    "context": ctx_prop(),
                }),
                &["kind", "name"],
            )
        },
        mutating: true,
        destructive: true,
        narrow: "",
    },
    Tool {
        name: "pod_logs",
        description: "The last lines of one pod's container (kubectl logs). previous: true for the run before the last restart. Read-only. For searching across pods, namespaces or deleted containers use logs_search.",
        schema: || {
            obj(
                json!({
                    "namespace": {"type": "string", "description": "default: default"},
                    "pod": {"type": "string"},
                    "container": {"type": "string", "description": "default: the only/first container"},
                    "previous": {"type": "boolean"},
                    "tail": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "default 100"},
                    "since_seconds": {"type": "integer", "minimum": 1},
                    "context": ctx_prop(),
                }),
                &["pod"],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Lower tail, or use logs_search with grep.",
    },
    Tool {
        name: "logs_search",
        description: "Search logs across pods and nodes with the built-in log search (also logs of containers that are gone): by namespace, label selector, pod-name regex, container, time range and a grep regex. Newest last. Read-only.",
        schema: || {
            obj(
                json!({
                    "namespace": {"type": "string", "description": "default: all"},
                    "selector": {"type": "string", "description": "pod label selector, e.g. app=web"},
                    "pod": {"type": "string", "description": "regex on pod names"},
                    "container": {"type": "string"},
                    "grep": {"type": "string", "description": "regex on lines; (?i) for any case, e.g. (?i)error|panic"},
                    "since": {"type": "string", "description": "duration ago (15m, 2h) or RFC 3339 time"},
                    "until": {"type": "string"},
                    "node": {"type": "string"},
                    "live_only": {"type": "boolean", "description": "leave out containers that are gone"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "newest N lines (default 200)"},
                    "context": ctx_prop(),
                }),
                &[],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Narrow with namespace, selector, pod, grep, since or a lower limit.",
    },
    Tool {
        name: "query",
        description: "Instant PromQL query on the cluster's built-in metrics (cAdvisor, node-exporter and kube-state-metrics names; see litek8s://skill/references/observability.md for recipes). Read-only.",
        schema: || obj(json!({"query": {"type": "string"}, "time": {"type": "string", "description": "RFC 3339 or unix seconds (default now)"}, "context": ctx_prop()}), &["query"]),
        mutating: false,
        destructive: false,
        narrow: "Aggregate (sum by (...), topk(10, ...)) or add label matchers.",
    },
    Tool {
        name: "query_range",
        description: "PromQL range query: a series of values over time (e.g. memory of a pod over the last hour). Read-only.",
        schema: || {
            obj(
                json!({
                    "query": {"type": "string"},
                    "range": {"type": "string", "description": "how far back from now, e.g. 1h, 30m (default 1h)"},
                    "step": {"type": "string", "description": "e.g. 1m (default: range/60)"},
                    "context": ctx_prop(),
                }),
                &["query"],
            )
        },
        mutating: false,
        destructive: false,
        narrow: "Use a shorter range, a bigger step, or aggregate to fewer series.",
    },
    Tool {
        name: "alerts_list",
        description: "Alerts of the cluster (built-in rules: crash loops, OOM kills, NotReady nodes, full disks, ...; plus AlertRule objects): state, severity, target, summary, fingerprint, whether silenced. Also the silences. Read-only.",
        schema: || obj(json!({"all": {"type": "boolean", "description": "also resolved ones (kept 15 minutes)"}, "context": ctx_prop()}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "alert_silence",
        description: "Silence alerts matching label matchers for a while (creates a Silence object). Matchers: name=value, name!=value, name=~regex, name!~regex, e.g. [\"alertname=KubePodCrashLooping\", \"namespace=shop\"].",
        schema: || {
            obj(
                json!({
                    "matchers": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "duration": {"type": "string", "description": "e.g. 30m, 2h, 7d (default 2h)"},
                    "comment": {"type": "string"},
                    "context": ctx_prop(),
                }),
                &["matchers"],
            )
        },
        mutating: true,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "alert_unsilence",
        description: "Remove a silence by name (from alerts_list's silences).",
        schema: || obj(json!({"name": {"type": "string"}, "context": ctx_prop()}), &["name"]),
        mutating: true,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "applications_list",
        description: "Argo CD Applications (argoproj.io; served by Argo CD or any controller that reads its manifests): each one's project, source, destination, sync and health status, revision, automated or not, last operation and conditions. Read-only.",
        schema: || obj(json!({"namespace": {"type": "string", "description": "default: all namespaces"}, "context": ctx_prop()}), &[]),
        mutating: false,
        destructive: false,
        narrow: "Give a namespace.",
    },
    Tool {
        name: "application_status",
        description: "One Argo CD Application in detail: its summary, the resources that are out of sync, not healthy or to be pruned (with health messages), the running or last operation (phase, message, resources that failed), recent history and events. Answers why an app is OutOfSync or Degraded. Read-only.",
        schema: || obj(json!({"name": {"type": "string"}, "namespace": {"type": "string", "description": "the Application's namespace (default argocd)"}, "all_resources": {"type": "boolean", "description": "list every resource, not only the ones that need attention"}, "context": ctx_prop()}), &["name"]),
        mutating: false,
        destructive: false,
        narrow: "Leave all_resources off.",
    },
    Tool {
        name: "application_diff",
        description: "What differs between git and the cluster for an Argo CD Application: per resource out of sync, missing or to be pruned, a unified diff (- live, + git) of the fields git sets, Secret values masked. From the diff its controller publishes (a Secret <app>.diff next to the app); Argo CD itself does not publish one (then: argocd app diff). Read-only.",
        schema: || obj(json!({"name": {"type": "string"}, "namespace": {"type": "string", "description": "the Application's namespace (default argocd)"}, "context": ctx_prop()}), &["name"]),
        mutating: false,
        destructive: false,
        narrow: "Fix one resource at a time; application_status lists them.",
    },
    Tool {
        name: "application_sync",
        description: "Sync an Argo CD Application: writes the operation the argocd CLI writes (the app's sync options; prune: also delete what git no longer has; revision: another than the target). Its controller runs it; wait_seconds waits for the result. Changes the cluster: needs confirm: true; without it reports what is out of sync and would be pruned.",
        schema: || {
            obj(
                json!({
                    "name": {"type": "string"},
                    "namespace": {"type": "string", "description": "default argocd"},
                    "prune": {"type": "boolean", "description": "delete resources git no longer has (default false)"},
                    "revision": {"type": "string", "description": "a branch, tag or commit (default: the app's target revision)"},
                    "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 600, "description": "wait for the operation to finish (default 0: return at once)"},
                    "confirm": confirm_prop(),
                    "context": ctx_prop(),
                }),
                &["name"],
            )
        },
        mutating: true,
        destructive: true,
        narrow: "",
    },
    Tool {
        name: "application_refresh",
        description: "Ask an Argo CD Application's controller to compare it with git again now (hard: also render its manifests again, past caches), as the refresh annotation does; returns its sync and health after the refresh when wait_seconds allows.",
        schema: || obj(json!({"name": {"type": "string"}, "namespace": {"type": "string", "description": "default argocd"}, "hard": {"type": "boolean"}, "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 120, "description": "wait for the refresh (default 10)"}, "context": ctx_prop()}), &["name"]),
        mutating: true,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "docker_ps",
        description: "Containers of a dev cluster's Docker API (docker ps; these are runtime containers, not pods). Read-only.",
        schema: || obj(json!({"all": {"type": "boolean", "description": "also stopped ones"}, "cluster": cluster_prop()}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "docker_images",
        description: "Images in a dev cluster's image store (shared by pods and Docker containers). Read-only.",
        schema: || obj(json!({"cluster": cluster_prop()}), &[]),
        mutating: false,
        destructive: false,
        narrow: "",
    },
    Tool {
        name: "docker_logs",
        description: "The last lines of a Docker container's log on a dev cluster's Docker API. Read-only.",
        schema: || obj(json!({"container": {"type": "string", "description": "id or name"}, "tail": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "default 100"}, "cluster": cluster_prop()}), &["container"]),
        mutating: false,
        destructive: false,
        narrow: "Lower tail.",
    },
];

pub struct Server {
    args: McpArgs,
    /// API servers that passed the litek8s check, by URL, and when.
    verified: Mutex<HashMap<String, Instant>>,
    catalogs: Mutex<HashMap<String, Catalog>>,
}

/// A litek8s API server a call talks to.
pub struct Kube {
    pub ep: Endpoint,
    pub context: String,
}

impl Server {
    pub fn new(args: McpArgs) -> Self {
        Server { args, verified: Mutex::new(HashMap::new()), catalogs: Mutex::new(HashMap::new()) }
    }

    fn tools(&self) -> impl Iterator<Item = &'static Tool> + '_ {
        TOOLS.iter().filter(|t| !(self.args.read_only && t.mutating))
    }

    pub fn list(&self) -> Vec<Value> {
        self.tools()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": (t.schema)(),
                    "annotations": {"readOnlyHint": !t.mutating, "destructiveHint": t.destructive, "idempotentHint": !t.mutating || t.name == "apply", "openWorldHint": false},
                })
            })
            .collect()
    }

    /// Runs a tool; `None` for a tool it does not have.
    pub async fn call(&self, name: &str, args: &Value) -> Option<Value> {
        let tool = self.tools().find(|t| t.name == name)?;
        let args = if args.is_object() { args.clone() } else { json!({}) };
        Some(match self.dispatch(name, &args).await {
            Ok(v) => {
                let v = guard::fit(v, self.args.max_output, tool.narrow);
                json!({"content": [{"type": "text", "text": serde_json::to_string(&v).unwrap_or_default()}], "structuredContent": v, "isError": false})
            }
            Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
        })
    }

    async fn dispatch(&self, name: &str, a: &Value) -> Result<Value> {
        match name {
            "clusters_list" => Ok(crate::dev::list_json().await),
            "cluster_status" => crate::dev::status_json(&crate::dev::cluster::Cluster::new(&cluster_name(a))?).await,
            "cluster_up" => cluster_up(a).await,
            "cluster_down" => cluster_down(a).await,
            "cluster_delete" => cluster_delete(a).await,
            "contexts_list" => contexts_list(a).await,
            "docker_ps" | "docker_images" | "docker_logs" => docker(name, a).await,
            _ => {
                let k = self.kube(a).await?;
                match name {
                    "get" => self.get(&k, a).await,
                    "describe" => self.describe(&k, a).await,
                    "events" => events(&k, a).await,
                    "diagnose_pod" => self.diagnose(&k, a).await,
                    "apply" => self.apply(&k, a).await,
                    "delete" => self.delete(&k, a).await,
                    "pod_logs" => pod_logs(&k, a).await,
                    "logs_search" => logs_search(&k, a).await,
                    "query" | "query_range" => promql(&k, name == "query_range", a).await,
                    "alerts_list" => alerts_list(&k, a).await,
                    "alert_silence" => alert_silence(&k, a).await,
                    "applications_list" => self.applications_list(&k, a).await,
                    "application_status" => self.application_status(&k, a).await,
                    "application_diff" => self.application_diff(&k, a).await,
                    "application_sync" => self.application_sync(&k, a).await,
                    "application_refresh" => self.application_refresh(&k, a).await,
                    "alert_unsilence" => {
                        let n = req(a, "name")?;
                        k.ep.json(Method::DELETE, &format!("{}/{}", crate::obs::cli::SILENCES, enc(n)), None).await?;
                        Ok(json!({"deleted": n}))
                    }
                    _ => bail!("unknown tool {name}"),
                }
            }
        }
    }

    /// The call's API server, after the litek8s check.
    async fn kube(&self, a: &Value) -> Result<Kube> {
        let context = a.get("context").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string).or_else(|| self.args.context.clone());
        let (ep, context) = match &context {
            Some(c) => (crate::ui::resolve_kube(Some(c))?.0, c.clone()),
            None => (crate::obs::cli::endpoint(None)?, String::new()),
        };
        let fresh = self.verified.lock().await.get(&ep.display).is_some_and(|t| t.elapsed() < Duration::from_secs(60));
        if !fresh {
            guard::check_kube(&ep, &context).await?;
            self.verified.lock().await.insert(ep.display.clone(), Instant::now());
        }
        Ok(Kube { ep, context })
    }

    async fn catalog(&self, k: &Kube, refresh: bool) -> Result<Catalog> {
        if !refresh {
            if let Some(c) = self.catalogs.lock().await.get(&k.ep.display) {
                return Ok(c.clone());
            }
        }
        let c = Catalog::new(ops::discover(&k.ep).await?);
        self.catalogs.lock().await.insert(k.ep.display.clone(), c.clone());
        Ok(c)
    }

    /// The resource type a name means, rediscovering once (a CRD created a
    /// moment ago).
    async fn resource(&self, k: &Kube, kind: &str) -> Result<Resource> {
        if let Some(r) = self.catalog(k, false).await?.find(kind) {
            return Ok(r.clone());
        }
        self.catalog(k, true).await?.find(kind).cloned().with_context(|| format!("the server has no resource type {kind:?} (get kind=customresourcedefinitions lists CRDs)"))
    }

    async fn get(&self, k: &Kube, a: &Value) -> Result<Value> {
        let res = self.resource(k, req(a, "kind")?).await?;
        let ns = opt(a, "namespace");
        if let Some(name) = opt(a, "name") {
            let ns = if res.namespaced { ns.unwrap_or("default") } else { "" };
            let r = ObjRef { res: res.clone(), ns: ns.into(), name: name.into() };
            let mut o = ops::get(&k.ep, &r).await?;
            let uid = o.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
            let events = ops::events(&k.ep, &r, &uid).await;
            let owners = self.owners(k, &o).await;
            clean(&mut o);
            let mut out = json!({"object": o, "events": diagnose::compact_events(&events), "owners": owners});
            if res.kind == "Pod" && o.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] != "True")) {
                out["hint"] = json!("This pod is not Ready: diagnose_pod explains why.");
            }
            return Ok(out);
        }
        let mut q = Vec::new();
        if let Some(l) = opt(a, "label_selector") {
            q.push(format!("labelSelector={}", enc(l)));
        }
        if let Some(f) = opt(a, "field_selector") {
            q.push(format!("fieldSelector={}", enc(f)));
        }
        let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(200);
        q.push(format!("limit={limit}"));
        let path = format!("{}?{}", res.list_path(ns), q.join("&"));
        let full = a.get("full").and_then(Value::as_bool).unwrap_or(false);
        let scope = if res.namespaced { ns.map(|n| format!("namespace {n}")).unwrap_or_else(|| "all namespaces".into()) } else { "cluster".into() };
        if full {
            let mut l = k.ep.json(Method::GET, &path, None).await?;
            let mut items = l["items"].take();
            for i in items.as_array_mut().into_iter().flatten() {
                clean(i);
            }
            return Ok(json!({"kind": res.kind, "scope": scope, "items": items, "continue": l.pointer("/metadata/continue")}));
        }
        let t: Value = k.ep.json_accept(&path, TABLE_ACCEPT).await?;
        let cols: Vec<(String, i64)> = t["columnDefinitions"].as_array().into_iter().flatten().map(|c| (c["name"].as_str().unwrap_or_default().to_string(), c["priority"].as_i64().unwrap_or(0))).collect();
        if cols.is_empty() {
            // Not a Table: names only.
            let items: Vec<Value> = t["items"].as_array().into_iter().flatten().map(|i| json!({"namespace": i.pointer("/metadata/namespace"), "name": i.pointer("/metadata/name")})).collect();
            return Ok(json!({"kind": res.kind, "scope": scope, "items": items}));
        }
        let rows: Vec<Value> = t["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let mut m = serde_json::Map::new();
                if let Some(ns) = r.pointer("/object/metadata/namespace").and_then(Value::as_str) {
                    m.insert("Namespace".into(), json!(ns));
                }
                for (i, (c, prio)) in cols.iter().enumerate() {
                    let v = &r["cells"][i];
                    if *prio > 0 && (v.is_null() || v == "<none>" || v == "") {
                        continue;
                    }
                    m.insert(c.clone(), v.clone());
                }
                Value::Object(m)
            })
            .collect();
        let mut out = json!({"kind": res.kind, "scope": scope, "count": rows.len(), "rows": rows});
        if let Some(c) = t.pointer("/metadata/continue").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            out["more"] = json!(format!("more than {limit} items: raise limit or narrow the selection (continue token {c})"));
        }
        Ok(out)
    }

    /// ownerReferences upwards: Pod -> ReplicaSet -> Deployment.
    async fn owners(&self, k: &Kube, o: &Value) -> Vec<Value> {
        let mut chain = Vec::new();
        let mut cur = o.clone();
        for _ in 0..5 {
            let refs = cur.pointer("/metadata/ownerReferences").and_then(Value::as_array).cloned().unwrap_or_default();
            let Some(r) = refs.iter().find(|r| r["controller"] == true).or(refs.first()).cloned() else { break };
            let (kind, name) = (r["kind"].as_str().unwrap_or_default(), r["name"].as_str().unwrap_or_default());
            let mut entry = json!({"kind": kind, "name": name});
            let Ok(cat) = self.catalog(k, false).await else { break };
            let Some(res) = cat.by_kind(r["apiVersion"].as_str().unwrap_or_default(), kind).cloned() else {
                chain.push(entry);
                break;
            };
            let ns = cur.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
            match ops::get(&k.ep, &ObjRef { res, ns: ns.into(), name: name.into() }).await {
                Ok(owner) => {
                    if let Some(st) = workload_status(&owner) {
                        entry["status"] = json!(st);
                    }
                    chain.push(entry);
                    cur = owner;
                }
                Err(e) => {
                    entry["error"] = json!(format!("{e:#}"));
                    chain.push(entry);
                    break;
                }
            }
        }
        chain
    }

    async fn describe(&self, k: &Kube, a: &Value) -> Result<Value> {
        let res = self.resource(k, req(a, "kind")?).await?;
        let ns = if res.namespaced { opt(a, "namespace").unwrap_or("default") } else { "" };
        let r = ObjRef { res, ns: ns.into(), name: req(a, "name")?.into() };
        let lines = ops::describe(&k.ep, &r).await?;
        let o = ops::get(&k.ep, &r).await.unwrap_or(Value::Null);
        Ok(json!({"describe": lines, "owners": self.owners(k, &o).await}))
    }

    async fn diagnose(&self, k: &Kube, a: &Value) -> Result<Value> {
        let ns = opt(a, "namespace").unwrap_or("default");
        let lines = a.get("log_lines").and_then(Value::as_u64).unwrap_or(30);
        let pods: Vec<Value> = if let Some(p) = opt(a, "pod") {
            vec![k.ep.json(Method::GET, &format!("/api/v1/namespaces/{ns}/pods/{}", enc(p)), None).await?]
        } else if let Some(sel) = opt(a, "selector") {
            let l = k.ep.json(Method::GET, &format!("/api/v1/namespaces/{ns}/pods?labelSelector={}", enc(sel)), None).await?;
            let all = l["items"].as_array().cloned().unwrap_or_default();
            if all.is_empty() {
                bail!("no pods in namespace {ns} match {sel:?}; get kind=pods namespace={ns} lists them (wrong namespace or selector?)");
            }
            let not_ready: Vec<Value> = all.iter().filter(|p| !pod_ready(p)).take(3).cloned().collect();
            if not_ready.is_empty() {
                return Ok(json!({"summary": format!("all {} pods matching {sel:?} are Ready", all.len()), "pods": all.iter().map(|p| p.pointer("/metadata/name")).collect::<Vec<_>>()}));
            }
            not_ready
        } else {
            bail!("give pod or selector");
        };
        let mut out = Vec::new();
        for p in pods {
            out.push(self.diagnose_one(k, p, lines).await);
        }
        Ok(if out.len() == 1 { out.remove(0) } else { json!({"pods": out}) })
    }

    async fn diagnose_one(&self, k: &Kube, pod: Value, log_lines: u64) -> Value {
        let ns = pod.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("default").to_string();
        let name = pod.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
        let uid = pod.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
        let pods_res = self.resource(k, "pods").await.ok();
        let events = match &pods_res {
            Some(res) => ops::events(&k.ep, &ObjRef { res: res.clone(), ns: ns.clone(), name: name.clone() }, &uid).await,
            None => Vec::new(),
        };
        let node_name = pod.pointer("/spec/nodeName").and_then(Value::as_str).unwrap_or_default();
        let node = if node_name.is_empty() { None } else { k.ep.json(Method::GET, &format!("/api/v1/nodes/{}", enc(node_name)), None).await.ok() };
        let findings = diagnose::analyze(&pod, node.as_ref(), &events, chrono::Utc::now().timestamp());
        let mut logs = serde_json::Map::new();
        if log_lines > 0 {
            for c in pod.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten().chain(pod.pointer("/status/initContainerStatuses").and_then(Value::as_array).into_iter().flatten()) {
                let cname = c["name"].as_str().unwrap_or_default();
                let mut entry = json!({});
                let started = c.pointer("/state/running").is_some() || c.pointer("/state/terminated").is_some();
                if started {
                    entry["lines"] = match log_tail(k, &ns, &name, cname, false, log_lines).await {
                        Ok(l) => json!(l),
                        Err(e) => json!(format!("(no logs: {e:#})")),
                    };
                }
                if c["restartCount"].as_i64().unwrap_or(0) > 0 || c.pointer("/lastState/terminated").is_some() {
                    entry["previous_logs"] = match log_tail(k, &ns, &name, cname, true, log_lines).await {
                        Ok(l) => json!(l),
                        Err(e) => json!(format!("(no previous logs: {e:#})")),
                    };
                }
                if entry.as_object().is_some_and(|m| !m.is_empty()) {
                    logs.insert(cname.to_string(), entry);
                }
            }
        }
        let conditions: Vec<Value> = pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| {
                let mut v = json!({"type": c["type"], "status": c["status"]});
                for f in ["reason", "message"] {
                    if c.get(f).is_some_and(|x| x.as_str().is_some_and(|s| !s.is_empty())) {
                        v[f] = c[f].clone();
                    }
                }
                v
            })
            .collect();
        let node_ready = node.as_ref().map(|n| n.pointer("/status/conditions").and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c["type"] == "Ready")).map(|c| c["status"].clone()).unwrap_or(Value::Null));
        json!({
            "pod": format!("{ns}/{name}"),
            "ready": pod_ready(&pod),
            "phase": pod.pointer("/status/phase"),
            "findings": findings,
            "conditions": conditions,
            "node": if node_name.is_empty() { Value::Null } else { json!({"name": node_name, "ready": node_ready}) },
            "podIP": pod.pointer("/status/podIP"),
            "containers": diagnose::container_summary(&pod),
            "owners": self.owners(k, &pod).await,
            "events": diagnose::compact_events(&events),
            "logs": logs,
        })
    }

    async fn apply(&self, k: &Kube, a: &Value) -> Result<Value> {
        let text = match (opt(a, "manifest"), opt(a, "path")) {
            (Some(m), _) => m.to_string(),
            (None, Some(p)) => std::fs::read_to_string(p).with_context(|| format!("cannot read {p}"))?,
            (None, None) => bail!("give manifest or path"),
        };
        let docs = parse_manifests(&text)?;
        if docs.is_empty() {
            bail!("no objects in the manifest");
        }
        let dry = a.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
        let default_ns = opt(a, "namespace").unwrap_or("default");
        let mut results = Vec::new();
        let mut failed = 0;
        for d in docs {
            let api_version = d["apiVersion"].as_str().unwrap_or_default().to_string();
            let kind = d["kind"].as_str().unwrap_or_default().to_string();
            let name = d.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut r = json!({"kind": kind, "name": name});
            match self.apply_one(k, d, &api_version, &kind, default_ns, dry).await {
                Ok((ns, action)) => {
                    if !ns.is_empty() {
                        r["namespace"] = json!(ns);
                    }
                    r["result"] = json!(action);
                }
                Err(e) => {
                    failed += 1;
                    r["error"] = json!(format!("{e:#}"));
                }
            }
            results.push(r);
        }
        let mut out = json!({"dry_run": dry, "objects": results});
        if failed > 0 {
            out["failed"] = json!(failed);
        }
        Ok(out)
    }

    async fn apply_one(&self, k: &Kube, mut d: Value, api_version: &str, kind: &str, default_ns: &str, dry: bool) -> Result<(String, &'static str)> {
        if api_version.is_empty() || kind.is_empty() {
            bail!("apiVersion and kind are required");
        }
        let name = d.pointer("/metadata/name").and_then(Value::as_str).filter(|n| !n.is_empty()).context("metadata.name is required (generateName is not supported by apply)")?.to_string();
        let find = |c: &Catalog| c.list.iter().find(|r| r.api_version() == api_version && r.kind == kind).cloned();
        let mut res = find(&self.catalog(k, false).await?);
        // A CRD applied a moment ago takes a little while to be served.
        let deadline = Instant::now() + Duration::from_secs(10);
        while res.is_none() && Instant::now() < deadline {
            res = find(&self.catalog(k, true).await?);
            if res.is_none() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        let res = res.with_context(|| format!("the server does not serve {kind} in {api_version} (a CRD missing or not established yet?)"))?;
        let ns = if res.namespaced {
            let ns = d.pointer("/metadata/namespace").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(default_ns).to_string();
            d["metadata"]["namespace"] = json!(ns);
            ns
        } else {
            String::new()
        };
        let path = res.object_path(&ns, &name);
        let before = k.ep.json(Method::GET, &path, None).await.ok();
        let mut q = format!("{path}?fieldManager={FIELD_MANAGER}&force=true");
        if dry {
            q.push_str("&dryRun=All");
        }
        let after = k.ep.json_typed(Method::PATCH, &q, "application/apply-patch+yaml", Some(&d)).await?;
        let action = match &before {
            None => {
                if dry {
                    "would be created"
                } else {
                    "created"
                }
            }
            // Not the resourceVersion: a controller may write the object
            // (status, annotations) between the GET and the apply.
            Some(b) if same_where_declared(&d, b, &after) => "unchanged",
            Some(_) if dry => "would be configured",
            Some(_) => "configured",
        };
        Ok((ns, action))
    }

    async fn delete(&self, k: &Kube, a: &Value) -> Result<Value> {
        let res = self.resource(k, req(a, "kind")?).await?;
        let ns = if res.namespaced { opt(a, "namespace").unwrap_or("default") } else { "" };
        let name = req(a, "name")?;
        let r = ObjRef { res: res.clone(), ns: ns.into(), name: name.into() };
        // It must exist either way; the preview says what goes with it.
        let o = ops::get(&k.ep, &r).await?;
        let mut would = json!({"would_delete": format!("{}/{}", res.kind, name), "context": k.context});
        if !ns.is_empty() {
            would["namespace"] = json!(ns);
        }
        if res.kind == "Namespace" {
            would["note"] = json!("deleting a namespace deletes everything in it");
        } else if let Some(n) = o.pointer("/spec/replicas") {
            would["note"] = json!(format!("its {n} replicas (and their pods) go with it"));
        }
        if let Err(preview) = guard::confirmed(a, would) {
            return Ok(preview);
        }
        let force = a.get("force").and_then(Value::as_bool).unwrap_or(false);
        let msg = ops::delete(&k.ep, &[r], Propagation::Background, force).await?;
        Ok(json!({"deleted": true, "message": msg}))
    }
}

/// Argo CD's Applications.
impl Server {
    async fn app_resource(&self, k: &Kube) -> Result<Resource> {
        let find = |c: &Catalog| c.list.iter().find(|r| gitops::is_app(r)).cloned();
        if let Some(r) = find(&self.catalog(k, false).await?) {
            return Ok(r);
        }
        find(&self.catalog(k, true).await?).context("this cluster has no Argo CD Applications (no applications.argoproj.io CRD)")
    }

    async fn app(&self, k: &Kube, a: &Value) -> Result<(ObjRef, Value)> {
        let r = ObjRef { res: self.app_resource(k).await?, ns: opt(a, "namespace").unwrap_or("argocd").into(), name: req(a, "name")?.into() };
        let app = ops::get(&k.ep, &r).await?;
        Ok((r, app))
    }

    async fn applications_list(&self, k: &Kube, a: &Value) -> Result<Value> {
        let res = self.app_resource(k).await?;
        let l = k.ep.json(Method::GET, &res.list_path(opt(a, "namespace")), None).await?;
        let items: Vec<Value> = l["items"].as_array().into_iter().flatten().map(gitops::summary).collect();
        let attention = items.iter().filter(|i| i["sync"] != "Synced" || i["health"] != "Healthy").count();
        Ok(json!({"applications": items, "not_synced_or_healthy": attention}))
    }

    async fn application_diff(&self, k: &Kube, a: &Value) -> Result<Value> {
        let (_, app) = self.app(k, a).await?;
        let ns = app.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
        let name = app.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
        let sync = app.pointer("/status/sync/status").cloned().unwrap_or(Value::Null);
        let Some(doc) = gitops::diff_doc(&k.ep, ns, name).await? else {
            return Ok(json!({"application": name, "sync": sync, "published": false, "message": gitops::diff_lines(&app, None).join("\n")}));
        };
        let resources: Vec<Value> = doc["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let yaml = |v: &Value| if v.is_null() { String::new() } else { crate::ui::kube_yaml::to_yaml(v, false) };
                let mut x = json!({"group": r["group"], "kind": r["kind"], "namespace": r["namespace"], "name": r["name"], "status": r["status"]});
                if r["omitted"] == true {
                    x["omitted"] = json!("too big to publish");
                } else {
                    x["diff"] = json!(crate::ui::kube_yaml::unified(&yaml(&r["live"]), &yaml(&r["desired"]), 3).join("\n"));
                }
                x
            })
            .collect();
        Ok(json!({"application": name, "sync": sync, "published": true, "revisions": doc["revisions"], "resources": resources}))
    }

    async fn application_status(&self, k: &Kube, a: &Value) -> Result<Value> {
        let (r, mut app) = self.app(k, a).await?;
        let catalog = self.catalog(k, false).await?;
        let find = |group: &str, kind: &str| catalog.list.iter().find(|x| x.group == group && x.kind == kind).cloned();
        gitops::fill_health(&k.ep, &mut app, &find).await;
        let all = a.get("all_resources").and_then(Value::as_bool).unwrap_or(false);
        let resources = gitops::resources(&app);
        let total = resources.len();
        let shown: Vec<Value> = resources
            .into_iter()
            .filter(|x| all || x["sync"] != "Synced" || x.get("health").is_some_and(|h| h != "Healthy") || x.get("requiresPruning").is_some())
            .collect();
        let mut out = json!({"application": gitops::summary(&app), "resources_total": total});
        out[if all { "resources" } else { "resources_needing_attention" }] = json!(shown);
        if let Some(op) = app.pointer("/status/operationState") {
            let failed: Vec<Value> = op
                .pointer("/syncResult/resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|x| x["status"] != "Synced" || (x.get("hookType").is_some() && x.get("hookPhase").is_some_and(|p| p != "Succeeded")))
                .map(|x| json!({"kind": x["kind"], "namespace": x["namespace"], "name": x["name"], "status": x["status"], "message": x["message"], "hookType": x["hookType"], "hookPhase": x["hookPhase"]}))
                .collect();
            out["operation"] = json!({"phase": op["phase"], "message": op["message"], "initiatedBy": op.pointer("/operation/initiatedBy"), "revision": op.pointer("/syncResult/revision"), "retryCount": op["retryCount"], "startedAt": op["startedAt"], "finishedAt": op["finishedAt"], "resources_not_synced": failed});
        }
        if let Some(h) = app.pointer("/status/history").and_then(Value::as_array) {
            out["history"] = json!(h.iter().rev().take(3).map(|e| json!({"id": e["id"], "revision": e["revision"], "revisions": e["revisions"], "deployedAt": e["deployedAt"]})).collect::<Vec<_>>());
        }
        let uid = app.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
        out["events"] = json!(diagnose::compact_events(&ops::events(&k.ep, &r, &uid).await));
        Ok(out)
    }

    async fn application_sync(&self, k: &Kube, a: &Value) -> Result<Value> {
        let (r, app) = self.app(k, a).await?;
        let prune = a.get("prune").and_then(Value::as_bool).unwrap_or(false);
        let revision = opt(a, "revision");
        let res = gitops::resources(&app);
        let out_of_sync: Vec<String> = res.iter().filter(|x| x["sync"] == "OutOfSync" && x.get("requiresPruning").is_none()).map(res_label).collect();
        let to_prune: Vec<String> = res.iter().filter(|x| x.get("requiresPruning").is_some()).map(res_label).collect();
        let mut would = json!({"would_sync": r.label(), "context": k.context, "operation": gitops::sync_operation(&app, "litek8s-mcp", prune, false, revision), "out_of_sync": out_of_sync});
        if prune {
            would["would_prune"] = json!(to_prune);
        } else if !to_prune.is_empty() {
            would["note"] = json!(format!("{} resources git no longer has stay (prune: true deletes them)", to_prune.len()));
        }
        if let Err(preview) = guard::confirmed(a, would) {
            return Ok(preview);
        }
        let msg = gitops::sync(&k.ep, &r, "litek8s-mcp", prune, false, revision).await?;
        let wait = a.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0);
        if wait == 0 {
            return Ok(json!({"started": true, "message": msg, "hint": "application_status shows its progress"}));
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let app = ops::get(&k.ep, &r).await?;
            let done = !app["operation"].is_object() && app.pointer("/status/operationState/phase").and_then(Value::as_str).is_some_and(|p| p != "Running" && p != "Terminating");
            if done || Instant::now() >= deadline {
                let mut out = json!({"started": true, "finished": done, "application": gitops::summary(&app)});
                if !done {
                    out["hint"] = json!("still running: application_status shows its progress");
                }
                return Ok(out);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn application_refresh(&self, k: &Kube, a: &Value) -> Result<Value> {
        let (r, _) = self.app(k, a).await?;
        let hard = a.get("hard").and_then(Value::as_bool).unwrap_or(false);
        let msg = gitops::refresh(&k.ep, &r, hard).await?;
        let wait = a.get("wait_seconds").and_then(Value::as_u64).unwrap_or(10);
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let app = ops::get(&k.ep, &r).await?;
            // The controller drops the annotation once it has refreshed.
            let pending = app.pointer(&format!("/metadata/annotations/{}", gitops::REFRESH.replace('/', "~1"))).is_some();
            if !pending || Instant::now() >= deadline {
                let mut out = json!({"message": msg, "refreshed": !pending, "application": gitops::summary(&app)});
                if pending {
                    out["hint"] = json!("not refreshed yet: is its controller (Argo CD or another) running?");
                }
                return Ok(out);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn res_label(x: &Value) -> String {
    let ns = x["namespace"].as_str().filter(|n| !n.is_empty()).map(|n| format!("{n}/")).unwrap_or_default();
    format!("{}/{ns}{}", x["kind"].as_str().unwrap_or_default(), x["name"].as_str().unwrap_or_default())
}

fn opt<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn req<'a>(a: &'a Value, k: &str) -> Result<&'a str> {
    opt(a, k).with_context(|| format!("{k} is required"))
}

fn cluster_name(a: &Value) -> String {
    opt(a, "name").or(opt(a, "cluster")).unwrap_or(crate::dev::cluster::DEFAULT).to_string()
}

/// The noise an agent does not need: managedFields, last-applied.
fn clean(o: &mut Value) {
    if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
        if let Some(a) = m.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
            if a.is_empty() {
                m.remove("annotations");
            }
        }
    }
}

fn pod_ready(p: &Value) -> bool {
    p.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

/// `ready 2/3` for a workload, from its status.
fn workload_status(o: &Value) -> Option<String> {
    let st = o.get("status")?;
    let want = o.pointer("/spec/replicas").and_then(Value::as_i64).or(st["desiredNumberScheduled"].as_i64())?;
    let ready = st["readyReplicas"].as_i64().or(st["numberReady"].as_i64()).unwrap_or(0);
    Some(format!("ready {ready}/{want}"))
}

/// The documents of a YAML or JSON manifest (`List`s unpacked).
pub fn parse_manifests(text: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(text) {
        let v: Value = serde::Deserialize::deserialize(doc).context("not valid YAML")?;
        match v {
            Value::Null => {}
            Value::Object(ref m) if m.get("kind").and_then(Value::as_str).is_some_and(|k| k.ends_with("List")) && m.get("items").is_some_and(Value::is_array) => {
                out.extend(m["items"].as_array().cloned().unwrap_or_default());
            }
            Value::Object(_) => out.push(v),
            other => bail!("a document is not an object: {other}"),
        }
    }
    Ok(out)
}

async fn log_tail(k: &Kube, ns: &str, pod: &str, container: &str, previous: bool, tail: u64) -> Result<Vec<String>> {
    let mut path = format!("/api/v1/namespaces/{ns}/pods/{}/log?tailLines={tail}", enc(pod));
    if !container.is_empty() {
        path.push_str(&format!("&container={}", enc(container)));
    }
    if previous {
        path.push_str("&previous=true");
    }
    let resp = tokio::time::timeout(Duration::from_secs(20), k.ep.send(Method::GET, &path, None, Vec::new())).await.context("timed out")??;
    let status = resp.status();
    let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
    crate::ui::client::check(status, &body)?;
    Ok(String::from_utf8_lossy(&body).lines().map(|l| crate::ui::docker::printable(l.as_bytes())).collect())
}

async fn pod_logs(k: &Kube, a: &Value) -> Result<Value> {
    let ns = opt(a, "namespace").unwrap_or("default");
    let pod = req(a, "pod")?;
    let mut path_extra = String::new();
    if let Some(s) = a.get("since_seconds").and_then(Value::as_u64) {
        path_extra = format!("&sinceSeconds={s}");
    }
    let tail = a.get("tail").and_then(Value::as_u64).unwrap_or(100);
    let previous = a.get("previous").and_then(Value::as_bool).unwrap_or(false);
    let container = opt(a, "container").unwrap_or_default();
    let lines = if path_extra.is_empty() {
        log_tail(k, ns, pod, container, previous, tail).await?
    } else {
        let mut path = format!("/api/v1/namespaces/{ns}/pods/{}/log?tailLines={tail}{path_extra}", enc(pod));
        if !container.is_empty() {
            path.push_str(&format!("&container={}", enc(container)));
        }
        if previous {
            path.push_str("&previous=true");
        }
        let resp = k.ep.send(Method::GET, &path, None, Vec::new()).await?;
        let status = resp.status();
        let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
        crate::ui::client::check(status, &body)?;
        String::from_utf8_lossy(&body).lines().map(|l| crate::ui::docker::printable(l.as_bytes())).collect()
    };
    Ok(json!({"pod": format!("{ns}/{pod}"), "container": if container.is_empty() { Value::Null } else { json!(container) }, "previous": previous, "lines": lines}))
}

async fn logs_search(k: &Kube, a: &Value) -> Result<Value> {
    let mut q = Vec::new();
    for (key, param) in [("namespace", "namespace"), ("selector", "selector"), ("pod", "pod"), ("container", "container"), ("grep", "grep"), ("since", "since"), ("until", "until"), ("node", "node")] {
        if let Some(v) = opt(a, key) {
            q.push(format!("{param}={}", enc(v)));
        }
    }
    q.push(format!("limit={}", a.get("limit").and_then(Value::as_u64).unwrap_or(200)));
    if a.get("live_only").and_then(Value::as_bool) == Some(true) {
        q.push("liveOnly=true".into());
    }
    let resp = tokio::time::timeout(Duration::from_secs(90), k.ep.send(Method::GET, &format!("/litek8s/v1/logs?{}", q.join("&")), None, Vec::new())).await.context("timed out")??;
    let status = resp.status();
    if status == hyper::StatusCode::NOT_FOUND {
        bail!("this server has no log search (started with --no-observability?); use pod_logs");
    }
    if !status.is_success() {
        let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap_or_default();
        bail!("{}", v["error"].as_str().or(v["message"].as_str()).map(str::to_string).unwrap_or_else(|| format!("{status}: {}", String::from_utf8_lossy(&body))));
    }
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let mut body = Lines::new(resp.into_body());
    while let Some(l) = body.next().await {
        let Ok(v) = serde_json::from_slice::<Value>(&l) else { continue };
        if let Some(e) = v.get("error") {
            errors.push(json!({"node": v["node"], "error": e}));
            continue;
        }
        let mut row = json!({"ts": v["ts"], "pod": format!("{}/{}", v["namespace"].as_str().unwrap_or_default(), v["pod"].as_str().unwrap_or_default()), "container": v["container"], "line": v["line"]});
        if v["stream"] == "stderr" {
            row["stderr"] = json!(true);
        }
        lines.push(row);
    }
    let mut out = json!({"count": lines.len(), "lines": lines});
    if !errors.is_empty() {
        out["node_errors"] = json!(errors);
    }
    if out["count"] == 0 {
        out["hint"] = json!("No lines. Widen since (default: all kept logs), check namespace/selector with get kind=pods, or drop grep.");
    }
    Ok(out)
}

async fn promql(k: &Kube, range: bool, a: &Value) -> Result<Value> {
    let query = req(a, "query")?;
    let path = if range {
        let now = crate::obs::tsdb::now_ms() as f64 / 1000.0;
        let r = crate::obs::tsdb::parse_duration(opt(a, "range").unwrap_or("1h"))?.as_secs_f64();
        let step = match opt(a, "step") {
            Some(s) => crate::obs::tsdb::parse_duration(s)?.as_secs_f64(),
            None => (r / 60.0).max(10.0),
        };
        format!("/prometheus/api/v1/query_range?query={}&start={}&end={now}&step={step}", enc(query), now - r)
    } else {
        let mut p = format!("/prometheus/api/v1/query?query={}", enc(query));
        if let Some(t) = opt(a, "time") {
            p.push_str(&format!("&time={}", enc(t)));
        }
        p
    };
    let resp = tokio::time::timeout(Duration::from_secs(60), k.ep.send(Method::GET, &path, None, Vec::new())).await.context("timed out")??;
    let status = resp.status();
    let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
    if status == hyper::StatusCode::NOT_FOUND {
        bail!("this server has no metrics API (started with --no-observability?)");
    }
    let v: Value = serde_json::from_slice(&body).with_context(|| format!("{status}: not JSON"))?;
    if v["status"] != "success" {
        bail!("{}", v["error"].as_str().unwrap_or("query failed"));
    }
    let mut out = json!({"resultType": v["data"]["resultType"], "result": v["data"]["result"]});
    if v["data"]["result"].as_array().is_some_and(|r| r.is_empty()) {
        out["hint"] = json!("Empty result: the metric may not exist yet (samples every 10s), or the matchers match nothing. query 'count by (__name__) ({__name__=~\"kube_pod.*\"})' lists names.");
    }
    if let Some(w) = v.get("warnings") {
        out["warnings"] = w.clone();
    }
    Ok(out)
}

async fn alerts_list(k: &Kube, a: &Value) -> Result<Value> {
    let all = a.get("all").and_then(Value::as_bool).unwrap_or(false);
    let v = k.ep.json(Method::GET, "/litek8s/v1/alerts", None).await.map_err(|e| e.context("no alerts API (observability off?)"))?;
    let alerts: Vec<Value> = v["alerts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| all || r["state"] != "resolved")
        .map(|r| {
            let since = r["active_at"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            json!({
                "alertname": r["labels"]["alertname"],
                "state": r["state"],
                "severity": r["labels"]["severity"],
                "labels": r["labels"],
                "summary": r["annotations"]["summary"],
                "description": r["annotations"]["description"],
                "since": since,
                "fingerprint": r["fingerprint"],
                "silenced": r["silenced_by"].as_array().is_some_and(|s| !s.is_empty()),
            })
        })
        .collect();
    let silences: Vec<Value> = k
        .ep
        .json(Method::GET, crate::obs::cli::SILENCES, None)
        .await
        .ok()
        .and_then(|l| l["items"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|s| json!({"name": s.pointer("/metadata/name"), "matchers": s.pointer("/spec/matchers"), "endsAt": s.pointer("/spec/endsAt"), "comment": s.pointer("/spec/comment")}))
        .collect();
    let mut out = json!({"alerts": alerts, "silences": silences});
    for key in ["ruleErrors", "receiverErrors"] {
        if v[key].as_object().is_some_and(|o| !o.is_empty()) {
            out[key] = v[key].clone();
        }
    }
    Ok(out)
}

async fn alert_silence(k: &Kube, a: &Value) -> Result<Value> {
    let matchers: Vec<Value> = a["matchers"].as_array().context("matchers is required")?.iter().map(|m| crate::obs::cli::parse_matcher(m.as_str().unwrap_or_default())).collect::<Result<_>>()?;
    if matchers.is_empty() {
        bail!("give at least one matcher");
    }
    let d = crate::obs::tsdb::parse_duration(opt(a, "duration").unwrap_or("2h"))?;
    let obj = crate::obs::cli::silence_object(matchers, crate::obs::tsdb::now_ms() + d.as_millis() as i64, opt(a, "comment").unwrap_or("silenced by an agent (litek8s mcp)"), false);
    let created = k.ep.json(Method::POST, crate::obs::cli::SILENCES, Some(&obj)).await?;
    Ok(json!({"silence": created.pointer("/metadata/name"), "endsAt": created.pointer("/spec/endsAt"), "matchers": created.pointer("/spec/matchers")}))
}

async fn events(k: &Kube, a: &Value) -> Result<Value> {
    let mut fs = Vec::new();
    if let Some(kind) = opt(a, "kind") {
        fs.push(format!("involvedObject.kind={kind}"));
    }
    if let Some(n) = opt(a, "name") {
        fs.push(format!("involvedObject.name={n}"));
    }
    if a.get("warnings_only").and_then(Value::as_bool) == Some(true) {
        fs.push("type=Warning".into());
    }
    let base = match opt(a, "namespace") {
        Some(ns) => format!("/api/v1/namespaces/{ns}/events"),
        None => "/api/v1/events".into(),
    };
    let path = if fs.is_empty() { base } else { format!("{base}?fieldSelector={}", enc(&fs.join(","))) };
    let l = k.ep.json(Method::GET, &path, None).await?;
    let items = l["items"].as_array().cloned().unwrap_or_default();
    let mut ev = diagnose::compact_events(&items);
    let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let total = ev.len();
    if total > limit {
        ev.drain(..total - limit);
    }
    Ok(json!({"total": total, "events": ev}))
}

async fn contexts_list(a: &Value) -> Result<Value> {
    let cfg = crate::ui::kube_auth::load()?;
    let probe = a.get("probe").and_then(Value::as_bool).unwrap_or(false);
    let current = cfg.current();
    let mut rows = Vec::new();
    for name in cfg.contexts() {
        let cluster = cfg.named("contexts", &name).and_then(|c| c["context"]["cluster"].as_str().map(str::to_string)).unwrap_or_default();
        let server = cfg.named("clusters", &cluster).and_then(|c| c["cluster"]["server"].as_str().map(str::to_string)).unwrap_or_default();
        rows.push(json!({"name": name, "server": server, "current": current.as_deref() == Some(name.as_str())}));
    }
    if probe {
        let checks = rows.iter().map(|r| {
            let server = r["server"].as_str().unwrap_or_default().to_string();
            async move {
                // Anonymous and unverified: only /version, to tell which is litek8s.
                let Ok(ep) = Endpoint::parse_http(&server, None) else { return Value::Null };
                match tokio::time::timeout(Duration::from_secs(2), ep.json(Method::GET, "/version", None)).await {
                    Ok(Ok(v)) => json!(guard::is_litek8s_version(&v)),
                    _ => json!("unreachable"),
                }
            }
        });
        let results = futures_util::future::join_all(checks).await;
        for (r, l) in rows.iter_mut().zip(results) {
            r["litek8s"] = l;
        }
    }
    Ok(json!({"contexts": rows, "note": "Only litek8s contexts work with these tools; dev clusters are litek8s-<name>."}))
}

/// Runs `litek8s dev ...` as a child process (its output must not reach
/// this process's stdout, the MCP channel).
async fn dev_command(args: &[String], timeout: Duration) -> Result<Value> {
    let exe = std::env::current_exe().context("cannot find the litek8s binary")?;
    let child = tokio::process::Command::new(exe)
        .arg("dev")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, child).await.with_context(|| format!("litek8s dev {} did not finish within {}s", args.join(" "), timeout.as_secs()))??;
    let text = |b: &[u8]| -> Vec<String> {
        let s = String::from_utf8_lossy(b);
        let lines: Vec<String> = s.lines().map(|l| crate::ui::docker::printable(l.as_bytes())).filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(40)..].to_vec()
    };
    let mut v = json!({"command": format!("litek8s dev {}", args.join(" ")), "ok": out.status.success(), "exit_code": out.status.code(), "output": text(&out.stdout)});
    let err = text(&out.stderr);
    if !err.is_empty() {
        v["stderr"] = json!(err);
    }
    Ok(v)
}

async fn cluster_up(a: &Value) -> Result<Value> {
    let name = cluster_name(a);
    crate::dev::cluster::Cluster::new(&name)?;
    let timeout = a.get("timeout_seconds").and_then(Value::as_u64).unwrap_or(180);
    let mut args = vec!["up".to_string(), "--name".into(), name.clone(), "--timeout".into(), timeout.to_string()];
    if let Some(n) = a.get("nodes").and_then(Value::as_u64) {
        args.extend(["--nodes".into(), n.to_string()]);
    }
    if let Some(n) = opt(a, "node") {
        args.extend(["--node".into(), n.to_string()]);
    }
    if a.get("switch_context").and_then(Value::as_bool) != Some(true) {
        args.push("--no-switch".into());
    }
    let mut v = dev_command(&args, Duration::from_secs(timeout + 120)).await?;
    let cluster = crate::dev::cluster::Cluster::new(&name)?;
    v["status"] = crate::dev::status_json(&cluster).await?;
    if v["ok"] == true {
        v["hint"] = json!(format!("Pass context: \"{}\" to the other tools; Docker API at {}", cluster.context(), cluster.docker_socket().display()));
    }
    Ok(v)
}

async fn cluster_down(a: &Value) -> Result<Value> {
    let name = cluster_name(a);
    let cluster = crate::dev::cluster::Cluster::new(&name)?;
    let status = crate::dev::status_json(&cluster).await?;
    let mut would = json!({"would_stop": match opt(a, "node") { Some(n) => format!("node {n} of cluster {name}"), None => format!("cluster {name}") }, "status": status});
    if opt(a, "node").is_none() {
        would["note"] = json!("its pods stop; objects and images are kept for the next cluster_up");
    }
    if let Err(preview) = guard::confirmed(a, would) {
        return Ok(preview);
    }
    let mut args = vec!["down".to_string(), "--name".into(), name.clone()];
    if let Some(n) = opt(a, "node") {
        args.extend(["--node".into(), n.to_string()]);
    }
    dev_command(&args, Duration::from_secs(180)).await
}

async fn cluster_delete(a: &Value) -> Result<Value> {
    let name = cluster_name(a);
    let cluster = crate::dev::cluster::Cluster::new(&name)?;
    let status = crate::dev::status_json(&cluster).await?;
    if status["exists"] == false && status["running"] == false {
        bail!("there is no dev cluster {name:?} (clusters_list)");
    }
    let would = json!({"would_delete": format!("cluster {name}"), "status": status, "note": "objects, images, volumes and the kube context go too; this cannot be undone"});
    if let Err(preview) = guard::confirmed(a, would) {
        return Ok(preview);
    }
    dev_command(&["delete".to_string(), "--name".into(), name], Duration::from_secs(300)).await
}

async fn docker(tool: &str, a: &Value) -> Result<Value> {
    let cluster = crate::dev::cluster::Cluster::new(&cluster_name(a))?;
    let ep = Endpoint::unix(cluster.docker_socket());
    guard::check_docker(&ep).await?;
    match tool {
        "docker_ps" => {
            let all = a.get("all").and_then(Value::as_bool).unwrap_or(false);
            let l = ep.json(Method::GET, &format!("/containers/json?all={}", if all { 1 } else { 0 }), None).await?;
            let rows: Vec<Value> = l
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    let ports: Vec<String> = c["Ports"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|p| match p["PublicPort"].as_u64() {
                            Some(pp) => format!("{}:{}->{}/{}", p["IP"].as_str().unwrap_or("0.0.0.0"), pp, p["PrivatePort"], p["Type"].as_str().unwrap_or("tcp")),
                            None => format!("{}/{}", p["PrivatePort"], p["Type"].as_str().unwrap_or("tcp")),
                        })
                        .collect();
                    let mut r = json!({
                        "id": c["Id"].as_str().map(|s| &s[..s.len().min(12)]),
                        "names": c["Names"].as_array().map(|n| n.iter().filter_map(Value::as_str).map(|s| s.trim_start_matches('/')).collect::<Vec<_>>()),
                        "image": c["Image"],
                        "state": c["State"],
                        "status": c["Status"],
                        "ports": ports,
                    });
                    if let Some(p) = c["Labels"]["com.docker.compose.project"].as_str() {
                        r["compose_project"] = json!(p);
                    }
                    r
                })
                .collect();
            Ok(json!({"socket": ep.display, "containers": rows}))
        }
        "docker_images" => {
            let l = ep.json(Method::GET, "/images/json", None).await?;
            let rows: Vec<Value> = l
                .as_array()
                .into_iter()
                .flatten()
                .map(|i| json!({"id": i["Id"].as_str().map(|s| s.trim_start_matches("sha256:").chars().take(12).collect::<String>()), "tags": i["RepoTags"], "size_bytes": i["Size"], "created": i["Created"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|t| t.to_rfc3339())}))
                .collect();
            Ok(json!({"socket": ep.display, "images": rows}))
        }
        _ => {
            let id = req(a, "container")?;
            let info = ep.json(Method::GET, &format!("/containers/{}/json", enc(id)), None).await?;
            let tty = info.pointer("/Config/Tty").and_then(Value::as_bool).unwrap_or(false);
            let tail = a.get("tail").and_then(Value::as_u64).unwrap_or(100);
            let resp = tokio::time::timeout(Duration::from_secs(20), ep.send(Method::GET, &format!("/containers/{}/logs?stdout=1&stderr=1&tail={tail}", enc(id)), None, Vec::new())).await.context("timed out")??;
            let status = resp.status();
            let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
            crate::ui::client::check(status, &body)?;
            let mut text = Vec::new();
            if tty {
                text.extend_from_slice(&body);
            } else {
                crate::ui::docker::Demux::default().feed(&body, &mut text);
            }
            let lines: Vec<String> = String::from_utf8_lossy(&text).lines().map(|l| crate::ui::docker::printable(l.as_bytes())).collect();
            Ok(json!({"container": id, "state": info.pointer("/State/Status"), "exit_code": info.pointer("/State/ExitCode"), "lines": lines}))
        }
    }
}

/// Whether `before` and `after` agree on every field the applied manifest
/// declares (objects are walked key by key; lists and scalars compare whole),
/// so that only the apply's own effect decides "unchanged".
fn same_where_declared(declared: &Value, before: &Value, after: &Value) -> bool {
    match declared {
        Value::Object(fields) => fields.iter().all(|(k, v)| same_where_declared(v, &before[k.as_str()], &after[k.as_str()])),
        _ => before == after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_ignores_fields_the_manifest_does_not_declare() {
        use serde_json::json;
        let declared = json!({"metadata": {"name": "web", "labels": {"app": "web"}}, "spec": {"replicas": 2}});
        let before = json!({"metadata": {"name": "web", "labels": {"app": "web"}, "resourceVersion": "5"}, "spec": {"replicas": 2}, "status": {"replicas": 1}});
        let mut after = json!({"metadata": {"name": "web", "labels": {"app": "web"}, "resourceVersion": "7", "annotations": {"deployment.kubernetes.io/revision": "1"}}, "spec": {"replicas": 2}, "status": {"replicas": 2}});
        assert!(super::same_where_declared(&declared, &before, &after), "a controller's status write is not the apply's change");
        after["spec"]["replicas"] = json!(3);
        assert!(!super::same_where_declared(&declared, &before, &after));
    }

    #[test]
    fn every_tool_has_a_valid_schema() {
        let mut names = std::collections::HashSet::new();
        for t in TOOLS {
            assert!(names.insert(t.name), "duplicate tool {}", t.name);
            assert!(t.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", t.name);
            assert!(t.description.len() > 20, "{}", t.name);
            let s = (t.schema)();
            assert_eq!(s["type"], "object", "{}", t.name);
            let props = s["properties"].as_object().unwrap();
            for r in s["required"].as_array().unwrap() {
                assert!(props.contains_key(r.as_str().unwrap()), "{}: required {r} is not a property", t.name);
            }
            for (k, p) in props {
                assert!(p["type"].is_string(), "{}.{k} has no type", t.name);
            }
            // Destructive tools take confirm; read-only ones do not.
            assert_eq!(props.contains_key("confirm"), t.destructive, "{}", t.name);
            assert!(!t.destructive || t.mutating, "{}", t.name);
        }
        for must in ["clusters_list", "cluster_status", "cluster_up", "cluster_down", "get", "describe", "apply", "delete", "logs_search", "query", "query_range", "alerts_list", "alert_silence", "diagnose_pod", "docker_ps", "docker_images", "docker_logs"] {
            assert!(names.contains(must), "missing {must}");
        }
    }

    #[test]
    fn read_only_leaves_out_mutating_tools() {
        let s = Server::new(McpArgs { context: None, max_output: 1000, read_only: true });
        let names: Vec<String> = s.list().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for gone in ["apply", "delete", "cluster_up", "cluster_down", "cluster_delete", "alert_silence"] {
            assert!(!names.contains(&gone.to_string()), "{gone}");
        }
        assert!(names.contains(&"diagnose_pod".to_string()));
        let all = Server::new(McpArgs { context: None, max_output: 1000, read_only: false }).list();
        let del = all.iter().find(|t| t["name"] == "delete").unwrap();
        assert_eq!(del["annotations"]["destructiveHint"], true);
        assert_eq!(all.iter().find(|t| t["name"] == "get").unwrap()["annotations"]["readOnlyHint"], true);
    }

    #[tokio::test]
    async fn read_only_refuses_mutating_calls_and_unknown_tools() {
        let s = Server::new(McpArgs { context: None, max_output: 1000, read_only: true });
        assert!(s.call("delete", &json!({"kind": "pods", "name": "x", "confirm": true})).await.is_none());
        assert!(s.call("nope", &json!({})).await.is_none());
    }

    #[test]
    fn manifests_split_into_objects() {
        let docs = parse_manifests("---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: a}\n---\n# only a comment\n---\napiVersion: v1\nkind: List\nitems:\n- {apiVersion: v1, kind: Secret, metadata: {name: b}}\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1]["kind"], "Secret");
        let docs = parse_manifests(r#"{"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "x"}}"#).unwrap();
        assert_eq!(docs[0]["metadata"]["name"], "x");
        assert!(parse_manifests("- a\n- b\n").is_err());
        assert!(parse_manifests("a: [").is_err());
    }

    #[test]
    fn objects_lose_their_noise() {
        let mut o = json!({"metadata": {"name": "a", "managedFields": [{}], "annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{}"}}});
        clean(&mut o);
        assert_eq!(o, json!({"metadata": {"name": "a"}}));
        assert_eq!(workload_status(&json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 2}})).unwrap(), "ready 2/3");
        assert_eq!(workload_status(&json!({"spec": {}, "status": {"desiredNumberScheduled": 2, "numberReady": 2}})).unwrap(), "ready 2/2");
    }
}

