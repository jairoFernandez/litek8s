# Observability: metrics, logs, alerts (built in)

Every litek8s API server (and every `litek8s dev` cluster) has its own
time-series store, a Prometheus-compatible API, log search over the nodes'
CRI log files and an alert evaluator. No Prometheus, Loki or exporters to
install. Off only when started with `--no-observability`.

| | MCP tool | CLI | HTTP |
|---|---|---|---|
| instant query | `query` | `litek8s query 'expr' -o json` | `/prometheus/api/v1/query` |
| range query | `query_range range=1h step=1m` | `litek8s query 'expr' --range 1h --step 1m -o json` | `/prometheus/api/v1/query_range` |
| log search | `logs_search` | `litek8s logs [pod-regex] -n ns -l sel --grep re --since 1h -o json` | `/litek8s/v1/logs` |
| alerts | `alerts_list` | `litek8s alerts -o json` | `/litek8s/v1/alerts` |
| silence | `alert_silence matchers=[...] duration=2h` | `litek8s alerts silence alertname=X --for 2h` | Silence objects |

The CLI takes `--context litek8s-N`. Grafana: Prometheus data source at
`http://<server>/prometheus`.

## Metric names (what exists)

Samples every 10 s. Names follow cAdvisor, node-exporter and
kube-state-metrics, so familiar queries work:

- containers (`namespace`, `pod`, `container`, `image`, `node`):
  `container_cpu_usage_seconds_total`, `container_memory_working_set_bytes`,
  `container_memory_rss`, `container_memory_cache`,
  `container_spec_memory_limit_bytes`, `container_oom_events_total`,
  `container_cpu_cfs_throttled_periods_total`, `container_cpu_cfs_periods_total`,
  `container_fs_{reads,writes}_bytes_total`; per pod
  `container_network_{receive,transmit}_bytes_total`.
  Per-pod sums: filter `container!=""` to skip the pod-level series.
- nodes (`node`/`instance`): `node_cpu_seconds_total{mode}`,
  `node_memory_MemAvailable_bytes`, `node_memory_MemTotal_bytes`,
  `node_filesystem_{avail,size}_bytes`, `node_load1`,
  `node_network_{receive,transmit}_bytes_total`, `machine_cpu_cores`.
- objects: `kube_pod_status_phase{phase}`, `kube_pod_status_ready{condition}`,
  `kube_pod_container_status_restarts_total`,
  `kube_pod_container_status_waiting_reason{reason}`,
  `kube_pod_container_status_last_terminated_reason{reason}`,
  `kube_pod_container_status_last_terminated_exitcode`,
  `kube_pod_container_resource_{requests,limits}{resource}`,
  `kube_deployment_spec_replicas`, `kube_deployment_status_replicas_available`,
  `kube_statefulset_status_replicas_ready`, `kube_daemonset_status_number_ready`,
  `kube_node_status_condition{condition,status}`, `kube_job_status_failed`,
  `kube_persistentvolumeclaim_status_phase`.
- volumes: `kubelet_volume_stats_{used,capacity,available}_bytes{persistentvolumeclaim}`.
- the API server: `apiserver_request_total`, `process_resident_memory_bytes`.
- alerts: `ALERTS{alertname, alertstate}`.

Not there: your applications' own `/metrics` (litek8s does not scrape pods
yet), recording rules of kube-prometheus-stack (`node_namespace_pod_container:...`).
Listing names: `count by (__name__) ({__name__=~"kube_pod.*"})`.

## PromQL recipes

```promql
# Top 10 pods by memory (working set, MiB)
topk(10, sum by (namespace, pod) (container_memory_working_set_bytes{container!=""}) / 2^20)

# Top 10 pods by CPU (cores, 5 m average)
topk(10, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total{container!=""}[5m])))

# Memory use as a fraction of the limit (containers with a limit)
max by (namespace, pod, container) (container_memory_working_set_bytes{container!=""})
  / max by (namespace, pod, container) (container_spec_memory_limit_bytes{container!=""} > 0)

# CPU throttling ratio
sum by (namespace, pod) (rate(container_cpu_cfs_throttled_periods_total[5m]))
  / sum by (namespace, pod) (rate(container_cpu_cfs_periods_total[5m]))

# Restarts in the last hour
sum by (namespace, pod) (increase(kube_pod_container_status_restarts_total[1h])) > 0

# Pods waiting, by reason (CrashLoopBackOff, ImagePullBackOff, ...)
sum by (namespace, pod, reason) (kube_pod_container_status_waiting_reason) > 0

# Last termination reasons (OOMKilled, Error)
kube_pod_container_status_last_terminated_reason == 1

# Pods not ready
kube_pod_status_ready{condition="true"} == 0

# Pods by phase
sum by (phase) (kube_pod_status_phase)

# Deployments not fully available
kube_deployment_spec_replicas - kube_deployment_status_replicas_available > 0

# Nodes not Ready
kube_node_status_condition{condition="Ready",status="true"} == 0

# Node memory available (fraction)
node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes

# Node CPU busy (fraction)
1 - avg by (node) (rate(node_cpu_seconds_total{mode="idle"}[5m]))

# Disk free on each node's data directory (fraction)
node_filesystem_avail_bytes / node_filesystem_size_bytes

# Volumes over 80 % full
kubelet_volume_stats_used_bytes / kubelet_volume_stats_capacity_bytes > 0.8

# Pod network receive rate (bytes/s)
sum by (namespace, pod) (rate(container_network_receive_bytes_total[5m]))

# API server request rate by verb and code
sum by (verb, code) (rate(apiserver_request_total[5m]))

# Firing alerts
ALERTS{alertstate="firing"}
```

For a trend, use the same expression with `query_range` (e.g. `range=6h
step=5m`). Supported: selectors and regexes, `rate/irate/increase`,
`*_over_time`, aggregations with `by/without`, `topk/bottomk/quantile`,
binary operators with `on/ignoring/group_left`, `histogram_quantile`,
`label_replace`, math and time functions. Not supported: subqueries
(`[1h:5m]`), `@`, recording rules. Data older than 6 h comes from 1-minute
(then 10-minute) downsampled tiers: `max_over_time` there sees averages.

## Log search

`logs_search` reads every node's CRI log files, also of containers that
are gone (kept 7 days by default). Filters: `namespace`, `selector` (pod
labels), `pod` (regex on names), `container`, `grep` (regex; `(?i)` for any
case), `since`/`until` (`15m`, `2h`, RFC 3339), `node`, `live_only`,
`limit` (newest N, default 200 in the tool). Examples:

- errors of an app in the last hour: `selector=app=web grep="(?i)error|panic|exception" since=1h`
- what a deleted pod printed: `pod=^job-migrate- since=24h`
- one namespace's warnings: `namespace=shop grep="(?i)warn"`

One pod's latest lines: `pod_logs` (`previous: true` for the run before the
last restart), which uses the normal `pods/log` API.

## Alerts

Built-in rules (evaluated every 30 s): `KubePodCrashLooping`,
`KubeContainerOOMKilled`, `KubePodRestartLoop`, `KubePodNotReady` (Pending
or Unknown 15 m), `KubeNodeNotReady` (critical), `KubeNodePressure`,
`NodeFilesystemAlmostFull`, `NodeMemoryLow`,
`KubePersistentVolumeFillingUp`, `Litek8sMetricsSeriesRefused`.

Own rules: `AlertRule` objects (group `observability.litek8s.dev/v1`, the
`PrometheusRule` spec; existing `PrometheusRule` objects are read too).
Notifications: `AlertReceiver` objects (webhook in Alertmanager's format,
ntfy, Telegram). Silences: `alert_silence` (matchers `name=value`,
`name!=value`, `name=~regex`, `name!~regex`), `alert_unsilence`; CLI
`litek8s alerts ack <alertname|fingerprint>` mutes until it resolves.
