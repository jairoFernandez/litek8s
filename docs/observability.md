# Observability: metrics, logs and alerts, built in

A home or edge cluster wants to know what its pods cost, find the error in
last night's logs and get a message when a disk fills or a pod crash-loops.
The usual answer, kube-prometheus-stack plus Loki, costs more than litek8s
itself (see [the measurements](#measured)). litek8s has it built in:

- **Metrics**: every node agent samples its machine and every container
  (cgroup v2: CPU, memory split into anonymous and page cache, I/O, OOM
  kills, throttling; the pod's network on its veth; persistent volume use)
  every 10 s; the API server adds object state (kube-state-metrics' names)
  and its own request metrics. All of it goes into an embedded time-series
  store: Gorilla-compressed chunks, a write-ahead log, three resolution
  tiers (10 s for 6 h, 1 min for 7 d, 10 min for 90 d by default), a hard
  memory budget.
- **A Prometheus-compatible API** with a practical PromQL subset, so
  Grafana uses litek8s directly as a Prometheus data source, and dashboards
  written for cAdvisor, node-exporter and kube-state-metrics partly work
  unchanged. `/metrics` stays as it was for a real Prometheus, and
  `/prometheus/federate` exports everything.
- **Logs**: search and follow across every pod and node (also of
  containers that are gone) from the CRI log files the nodes already write;
  no log store, no shipper.
- **Alerts**: built-in rules (crash loops, OOM kills, restart loops, nodes
  not ready, node pressure, full disks and volumes, low memory), your own as
  `AlertRule` objects in Prometheus rule syntax, notifications to a webhook
  (Alertmanager's payload), ntfy or Telegram, silences and acks.
- **`litek8s ui`** shows it all in its Observe view (9).

Everything is on by default in `litek8s apiserver` and `litek8s dev`;
`--no-observability` turns it off.

## Quick start

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'   # every pod of app=web, all nodes
litek8s logs -n shop -f                                    # follow a namespace
litek8s query 'topk(5, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m])))'
litek8s alerts                                             # what is firing
litek8s alerts ack KubePodCrashLooping                     # mute until it resolves
```

Grafana: add a **Prometheus** data source with URL
`http://<api server>:6443/prometheus` (or start the API server with
`--prometheus-listen 0.0.0.0:9090` and use `http://<server>:9090`).

The CLI finds the API server like `litek8s ui`: `--context`, else
`$KUBECONFIG`'s current context, else the local dev cluster.

## Metrics

### What is collected

Every series carries `job` and `instance` labels as Prometheus' usual
scrape jobs would, so dashboards' selectors match.

| job | where | series (labels) |
|---|---|---|
| `kubelet`, `metrics_path="/metrics/cadvisor"` | each node agent, every container (crun: its cgroup; Docker runtime: CPU and working set only) | `container_cpu_usage_seconds_total`, `container_cpu_user_seconds_total`, `container_cpu_system_seconds_total`, `container_cpu_cfs_periods_total`, `container_cpu_cfs_throttled_periods_total`, `container_cpu_cfs_throttled_seconds_total`, `container_memory_working_set_bytes`, `container_memory_usage_bytes`, `container_memory_rss` (anonymous), `container_memory_cache` (page cache), `container_spec_memory_limit_bytes`, `container_oom_events_total`, `container_fs_reads_bytes_total`, `container_fs_writes_bytes_total` (`namespace`, `pod`, `container`, `image`, `node`); per pod: `container_network_{receive,transmit}_{bytes,packets}_total`, `..._packets_dropped_total` (`interface="eth0"`) |
| `kubelet`, `metrics_path="/metrics"` | each node agent, every mounted persistent volume claim | `kubelet_volume_stats_used_bytes`, `_capacity_bytes`, `_available_bytes` (`namespace`, `persistentvolumeclaim`) |
| `node-exporter` | each node agent | `node_cpu_seconds_total{mode}`, `node_memory_{MemTotal,MemAvailable,MemFree,Cached,Buffers,SwapTotal,SwapFree}_bytes`, `node_load{1,5,15}`, `node_filesystem_{size,free,avail}_bytes` (the node's data directory), `node_network_{receive,transmit}_{bytes,packets,errs}_total{device}`, `node_disk_{read,written}_bytes_total`, `node_disk_io_time_seconds_total`, `machine_cpu_cores`, `up` |
| `litek8s` | each node agent | where the node's resources go, as `litek8s ui`'s Home explains them: `litek8s_node_memory_bytes{owner="litek8s|pods|cache"}`, `litek8s_node_cpu_seconds_total{owner="litek8s|pods"}`, `litek8s_node_disk_bytes{category}` (every 15 min) |
| `kube-state-metrics` | the API server, from its object store | `kube_pod_info`, `kube_pod_status_phase`, `kube_pod_status_ready`, `kube_pod_status_scheduled`, `kube_pod_container_info`, `kube_pod_container_status_{restarts_total,ready,running,waiting,terminated}`, `..._waiting_reason`, `..._terminated_reason`, `..._last_terminated_reason`, `..._last_terminated_exitcode`, `kube_pod_container_resource_{requests,limits}`, `kube_node_info`, `kube_node_status_condition`, `kube_node_status_{capacity,allocatable}`, `kube_node_spec_unschedulable`, `kube_deployment_*`, `kube_statefulset_*`, `kube_daemonset_*`, `kube_job_status_*`, `kube_job_failed`, `kube_cronjob_*`, `kube_persistentvolumeclaim_*`, `kube_persistentvolume_*`, `kube_namespace_status_phase` |
| `apiserver` | the API server | its `/metrics`: `apiserver_request_total`, `apiserver_longrunning_requests` (kube-apiserver's names and labels), `litek8s_apiserver_request_duration_seconds` (histogram), `litek8s_store_*`, …; `process_resident_memory_bytes`, `process_cpu_seconds_total`, `up` |
| `litek8s` | the API server | the store's own health: `litek8s_tsdb_{series,head_bytes,index_bytes,disk_bytes,wal_bytes,blocks}`, `..._series_{created,refused,removed}_total`, `..._samples_{appended,out_of_order}_total` |
| – | the alert evaluator | `ALERTS{alertname, alertstate="pending|firing", …}` as Prometheus writes it |

Sampling is aligned to the interval (10 s by default: the first tier's
resolution), so timestamps are regular. A series that stops (a deleted
pod's, a node that disconnects) gets a staleness marker, as in Prometheus:
it leaves instant queries at once instead of lingering for 5 minutes.

### The store

`src/obs/tsdb/`, data under `<data dir>/obs/`:

- **Series** are interned label sets with an inverted index (label pair →
  series) in memory; their definitions are appended to `series.log`.
- **Samples** go to `wal.log` first (fsynced every interval), then to the
  series' in-memory **head chunk**: Gorilla compression, delta-of-delta
  timestamps (a regular sample costs one bit) and XOR-ed values (an
  unchanged value one bit). Every 20 minutes (or at 120 samples) heads are
  cut into the current window's **block** file and the WAL is truncated.
- **Blocks** are one file per tier and time window (2 h for the raw tier).
  An open block is append-only with a CRC per chunk; a torn write at its end
  (a crash mid-write) is cut off when it is opened again. When the window
  is over it is **sealed**: rewritten sorted by series with an index at the
  end, then renamed into place (atomically).
- **Tiers and downsampling.** Sealing a window also downsamples it into the
  next tier: counters (`*_total`, `*_count`, `*_sum`, `*_bucket`, or typed
  counter) keep the last value of each bucket, gauges the average; buckets
  are stamped at their start. Each tier deletes blocks older than its
  retention once they have been downsampled. A query reads the coarsest tier
  whose resolution is at most its step (and at most a third of its shortest
  `rate` window) among those whose retention covers its start, and fills the
  part the coarser tier has not received yet from the finer one, downsampled
  on the fly. Instant queries read the finest tier that covers them.
- **Crash safety.** Anything acknowledged is in the WAL; replay skips what
  already reached a block; a seal or a downsample interrupted by a crash
  leaves either the old or the new file, and duplicate samples are dropped
  when read. Tests: `survives_restart_through_wal_and_blocks`,
  `torn_wal_tail_is_ignored`, `open_append_reopen_seal_query`.
- **Memory budget.** Only the label index and one head chunk per series are
  in memory; blocks are read per query. `--obs-memory` (24 MiB; 8 MiB in a
  `litek8s dev` cluster) sets the series limit (two thirds of the budget at
  ~900 bytes a series: 18 000 series by default); new series beyond it are
  refused and counted (`litek8s_tsdb_series_refused_total`, and the
  built-in alert `Litek8sMetricsSeriesRefused`). A query loads at most 1
  million samples (16 MB while it runs). Series whose data has left every tier are dropped from
  the index (and `series.log` is compacted).

Flags of `litek8s apiserver`:

| flag | default | |
|---|---|---|
| `--no-observability` | | no metrics, log search or alerts |
| `--obs-retention` | `10s:6h,1m:7d,10m:90d` | tiers, `<resolution>:<retention>`; the first resolution is the sampling interval; each resolution a multiple of the previous one |
| `--obs-memory` | `24Mi` | memory for series and heads |
| `--obs-log-retention` | `7d` | how long each node keeps logs of removed containers |
| `--obs-log-max-size` | `512Mi` | the most each node keeps of them |
| `--no-builtin-alerts` | | only your own rules |
| `--prometheus-listen` | | also serve the Prometheus API at the root of this address |

### Querying: the PromQL subset

Supported:

- selectors with `=`, `!=`, `=~`, `!~` (regexes anchored, as in
  Prometheus), `{__name__=~"..."}`, range selectors `[5m]`, `offset`;
- `rate`, `irate`, `increase` (extrapolated like Prometheus, counter resets
  handled), `delta`, `idelta`, `changes`, `resets`, `deriv`,
  `predict_linear`, `{avg,sum,min,max,count,last,stddev,stdvar,quantile,present,absent}_over_time`;
- `sum`, `avg`, `min`, `max`, `count`, `group`, `stddev`, `stdvar`, `topk`,
  `bottomk`, `quantile`, `count_values`, with `by (…)` / `without (…)`
  before or after;
- `+ - * / % ^ atan2`, comparisons with and without `bool`, `and`, `or`,
  `unless`, vector matching with `on`/`ignoring` and
  `group_left`/`group_right` (with extra labels), unary minus;
- `histogram_quantile`, `label_replace`, `label_join`, `abs`, `ceil`,
  `floor`, `round`, `exp`, `ln`, `log2`, `log10`, `sqrt`, `sgn`, `clamp`,
  `clamp_min`, `clamp_max`, `time`, `timestamp` (the evaluation time),
  `vector`, `scalar`, `absent`, `sort`, `sort_desc`, `day_of_week`,
  `day_of_month`, `days_in_month`, `hour`, `minute`, `month`, `year`.

Not supported: subqueries (`x[1h:5m]`), the `@` modifier, native
histograms, recording rules, exemplars (always empty).

### HTTP API

Under `/prometheus` on the API port (and at the root of
`--prometheus-listen`), GET or POST (form-encoded), Prometheus' response
format:

| path | |
|---|---|
| `/api/v1/query` | `query`, `time` |
| `/api/v1/query_range` | `query`, `start`, `end`, `step` (11 000 points at most) |
| `/api/v1/series` | `match[]`, `start`, `end` |
| `/api/v1/labels`, `/api/v1/label/<name>/values` | `match[]`, `start`, `end` |
| `/api/v1/metadata` | types and help of the API server's own metrics |
| `/api/v1/rules`, `/api/v1/alerts` | the alert rules and active alerts |
| `/api/v1/status/buildinfo` | reports Prometheus 2.53 (Grafana picks its features by it) |
| `/federate` | `match[]`: the latest sample of every matching series, text format |

## Logs

There is no log store: a search goes to every node, and each node agent
reads the CRI log files it already writes (`<time> <stream> <F|P> <text>`,
rotated at 10 MiB). The agent filters (namespace, pod label selector, pod
name regex, container, time range, a regex on the text), keeps the newest
`limit` matches and streams them back; the API server merges them by time.
`--follow` keeps tailing (and picks up containers that start later).

Logs outlive their container: when the crun runtime removes one (its pod
was deleted, or it restarted twice), its files move to the node's archive,
`<node dir>/logs/<namespace>/<pod>/<container>.<attempt>.<uid>.log` with the
pod's labels next to them, and are searched too, until
`--obs-log-retention` (7 days) or `--obs-log-max-size` (512 MiB per node,
oldest first) removes them. `--live-only` skips them.

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'
litek8s logs api- -n prod -f                      # pods whose name matches api-
litek8s logs --grep '(?i)timeout' --since 2026-10-19T08:00:00Z --until 30m --json
```

API: `GET /litek8s/v1/logs?namespace=&selector=&pod=&container=&since=&until=&grep=&limit=&follow=&node=&liveOnly=`
answers JSON lines (`ts`, `namespace`, `pod`, `container`, `node`,
`stream`, `line`); a node that cannot answer adds `{"node": …, "error": …}`.
`since`/`until` are a duration back from now (`1h`) or RFC 3339.

On the Docker runtime (macOS without a dev VM) the agent reads logs through
Docker instead of files, and there is no archive.

## Alerts

Rules are evaluated every 30 s by the API server. An alert is *pending*
until its `for` has passed, then *firing*; it *resolves* when the
expression stops returning it (resolved alerts stay listed 15 minutes).
State is saved in `<data dir>/obs/alerts.json`: a restart neither forgets
an alert nor notifies it twice.

### Built-in rules

| alert | expression | for | severity |
|---|---|---|---|
| `KubePodCrashLooping` | `max_over_time(kube_pod_container_status_waiting_reason{reason="CrashLoopBackOff"}[5m]) >= 1` | 1m | warning |
| `KubeContainerOOMKilled` | last termination `OOMKilled` and a restart in the last 10 m | – | warning |
| `KubePodRestartLoop` | `increase(kube_pod_container_status_restarts_total[15m]) > 3` | – | warning |
| `KubePodNotReady` | Pending or Unknown | 15m | warning |
| `KubeNodeNotReady` | `kube_node_status_condition{condition="Ready",status="true"} == 0` | 2m | critical |
| `KubeNodePressure` | Disk/Memory/PID pressure condition | 2m | warning |
| `NodeFilesystemAlmostFull` | < 10 % of the data directory's filesystem free | 5m | warning |
| `NodeMemoryLow` | < 10 % of memory available | 5m | warning |
| `KubePersistentVolumeFillingUp` | < 15 % of a volume free | 2m | warning |
| `Litek8sMetricsSeriesRefused` | the store refused series (budget) | – | warning |

`--no-builtin-alerts` turns them off; silence single ones instead.

### Your rules: `AlertRule`

The spec is a `PrometheusRule`'s: groups of rules with `alert`, `expr`,
`for`, `labels`, `annotations` (`{{ $labels.x }}`, `{{ $value }}`,
`| humanize`, `| humanizePercentage`, `| humanize1024`). Existing
`monitoring.coreos.com/v1` `PrometheusRule` objects are read too when that
CRD is installed. Errors show in `litek8s alerts` and `/api/v1/rules`.

```yaml
apiVersion: observability.litek8s.dev/v1
kind: AlertRule
metadata: {name: web, namespace: shop}
spec:
  groups:
    - name: web
      rules:
        - alert: WebErrorsHigh
          expr: sum(rate(http_errors_total{app="web"}[5m])) > 1
          for: 5m
          labels: {severity: critical, team: web}
          annotations: {summary: "web serves {{ $value | humanize }} errors/s"}
```

(Metrics of your own applications need a source; litek8s does not scrape
pods yet, see Limits.)

### Where notifications go: `AlertReceiver`

Cluster-wide objects; every receiver gets every alert its `matchers` match
(all of them; no matchers: everything), batched per evaluation, again every
`repeatInterval` (4h) while firing, and once resolved (`sendResolved`,
default true). A failing receiver is retried with backoff (30 s to 10 min).

```yaml
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: phone}
spec:
  ntfy: {topic: my-cluster-alerts}            # url defaults to https://ntfy.sh; tokenSecretRef for auth
  matchers: [{name: severity, value: critical}]
---
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: telegram}
spec:
  telegram:
    chatId: "123456789"
    botTokenSecretRef: {namespace: kube-system, name: telegram-bot, key: token}
---
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: hook}
spec:
  webhook: {url: "http://alert-handler.ops.svc:8080/alerts"}   # Alertmanager's webhook JSON (version 4)
  repeatInterval: 1h
```

### Silences and acks

A `Silence` mutes the alerts its matchers (`name`, `value`, `isRegex`,
`isEqual`) match between `startsAt` and `endsAt`. An *ack* is a silence on
all of one alert's labels, marked `observability.litek8s.dev/until-resolved`:
the server deletes it when the alert resolves.

```bash
litek8s alerts silence alertname=KubePodCrashLooping namespace=dev --for 4h --comment deploying
litek8s alerts ack 3f2a9c1d04b7e6a1          # a fingerprint, or an alert name (all of its alerts)
litek8s alerts silences
litek8s alerts unsilence silence-x7k2p
kubectl get alertrules -A; kubectl get alertreceivers,silences
```

## In `litek8s ui`

The Observe view (9) shows the active alerts and the pods by CPU or memory
(`s`) with sparklines of the last 30 minutes; `l` opens every log of the
selected pod (gone containers included), `L` searches every pod's logs of
the last hour by regex, `a` acknowledges the selected pod's alerts, `A`
all of them. It samples every 10 s while it is on screen and not at all
otherwise (see [ui.md](ui.md#light-on-the-machine)).

## With several API servers

The store, its directory (`<data dir>/obs`) and the collectors are a
module of their own (`src/obs`), apart from the object store, so they can
follow the control plane's leader once the API server is replicated: the
object-state collector, alert evaluation and notifications must run on
one server only (they read the store and write `Silence` deletions), and
node agents' samples must reach that server (a follower would forward the
`Metrics` messages of the agents connected to it). Today every API server
runs them for the agents connected to it.

## Limits

- No scraping of application `/metrics` endpoints yet (annotations or
  ServiceMonitors): the series are the platform's own.
- No recording rules, subqueries or `@`; downsampled tiers keep one value
  per bucket (the last of counters, the mean of gauges), so `max_over_time`
  over old data sees averages.
- Log search reads files: searching days of logs of a chatty cluster takes
  as long as reading them (every node in parallel); there is no index.
- One API server evaluates alerts and stores metrics (see above).

## Measured

See [bench/RESULTS.md](../bench/RESULTS.md#observability-built-in-vs-kube-prometheus-stack--loki).
