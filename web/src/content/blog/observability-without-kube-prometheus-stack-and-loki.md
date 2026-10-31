---
title: "Metrics, logs and alerts without kube-prometheus-stack and Loki: 1,371 MiB to 1.8 MiB"
description: "On a small k3s node the usual stack added 1,371 MiB. litek8s's built-in metrics, logs and alerts added 1.8 MiB. What that buys, and what it leaves out."
date: 2026-10-30
tags: [observability, prometheus, loki, alerts, memory]
docs: [observability, numbers, limitations, ui]
translation: es/observabilidad-sin-kube-prometheus-stack-ni-loki
---

A small cluster needs three things to be run by a small team: to know what
its pods cost, to find the error in last night's logs, and to get a message
when a disk fills or a pod crash-loops. The standard answer is
kube-prometheus-stack plus Loki and a log shipper. On a small node, that
answer is the biggest thing running.

We measured it. Same workload, same kind of guest (2 vCPU, 3 GiB, no swap,
Ubuntu 24.04 under KVM): three idle nginx pods and two log generators
writing 10 lines a second each.

| | k3s + kube-prometheus-stack + Loki + Alloy | litek8s, built in |
|---|---|---|
| memory observability adds (PSS) | **1,371 MiB** | **+1.8 MiB** |
| CPU it adds, % of one vCPU | 4.2 % | +0.03 % |
| what you install | 2.1 GiB of images | nothing (+2.1 MiB of binary) |
| metrics on disk | 2.0–4.1 GB for 10 days | ≈ 10 MB for 90 days |
| logs on disk beyond the CRI files | ≈ 52 MiB/day | 0 |

That is with Grafana off on the k3s side, since litek8s has no Grafana
either. The setup, raw data and the script to rerun it are in
[bench/RESULTS.md](https://github.com/jairoFernandez/litek8s/blob/main/bench/RESULTS.md)
("Observability: built in vs kube-prometheus-stack + Loki"); the summary is
on [Numbers](/docs/numbers/).

Before you share that table: the two sides do not collect the same thing.
That part is further down, and it matters.

## Try it in a minute

Observability is on by default in `litek8s dev` and `litek8s apiserver`.
On a dev cluster:

```bash
litek8s dev up
kubectl create deployment web --image=nginx
```

Then, from the [observability docs](/docs/observability/):

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'   # every pod of app=web, all nodes
litek8s logs -n shop -f                                    # follow a namespace
litek8s query 'topk(5, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m])))'
litek8s alerts                                             # what is firing
litek8s alerts ack KubePodCrashLooping                     # mute until it resolves
litek8s top                                                # nodes and pods, live
kubectl top pods -A --containers                           # no metrics-server needed
```

No Helm chart, no operator, no CRDs to install first. `kubectl top` works
because the API server serves `metrics.k8s.io/v1beta1` itself, from the
same store.

## Where the 1.8 MiB goes

**Metrics.** Every node agent samples its machine and each container every
10 seconds from cgroup v2: CPU, memory split into anonymous and page cache,
I/O, OOM kills, throttling, the pod's network on its veth, volume use. The
API server adds object state under kube-state-metrics' names, and its own
request metrics. Series carry the `job` and `instance` labels Prometheus'
usual scrape jobs would, so dashboards' selectors match.

It all goes into an embedded time-series store: Gorilla-compressed chunks,
a write-ahead log, and three resolution tiers (10 s for 6 hours, 1 minute
for 7 days, 10 minutes for 90 days by default). Only the label index and
one head chunk per series live in memory; blocks are read per query. A hard
budget (`--obs-memory`, 24 MiB by default, 8 MiB in a dev cluster) caps the
number of series, and series refused over it are counted and raise a
built-in alert instead of growing memory.

On the bench workload it stored 1.60 bytes per sample. On a simulated
3-node cluster with 240 containers (5,560 series, six hours of 10 s
samples), a test in the repository measured 4 million samples a second
ingested, a 1 MiB index, 1.8 MiB of head chunks, and a one-hour select of
240 series in 6 ms.

**Logs.** There is no log store. The nodes already write CRI log files;
a search goes to every node, each agent filters its own files (namespace,
label selector, pod name, container, time range, a regex on the text) and
streams the newest matches back, and the API server merges them by time.
When a container is removed (its pod deleted, or it restarted twice), its
log files move to a per-node archive and stay searchable for 7 days or up
to 512 MiB per node, oldest first. So "what did the pod I deleted an hour
ago print" has an answer:

```bash
litek8s logs api- -n prod -f                      # pods whose name matches api-
litek8s logs --grep '(?i)timeout' --since 2026-10-19T08:00:00Z --until 30m --json
```

(The archive is a crun-runtime feature: on the Docker runtime, macOS
without a dev VM, logs are read through Docker and there is no archive.)

**Alerts.** The API server evaluates rules every 30 seconds. Ten come
built in: crash loops, OOM kills, restart loops, pods not ready, nodes not
ready, node pressure, full disks and volumes, low memory, and the store
refusing series. Yours are `AlertRule` objects whose spec is a
`PrometheusRule`'s; existing `monitoring.coreos.com/v1` PrometheusRules are
read too when that CRD is installed:

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

Notifications go to `AlertReceiver` objects: a webhook with Alertmanager's
payload, ntfy, or Telegram. Silences and acks (a silence that deletes
itself when the alert resolves) come from the CLI. Alert state is saved to
disk, so a restart neither forgets an alert nor sends it twice.

## Grafana still works

The store answers Prometheus' HTTP API under `/prometheus` on the API port:
`query`, `query_range`, `series`, `labels`, `rules`, `alerts`, and
`/federate` to export everything to a real Prometheus. Add a Prometheus
data source with URL `http://<api server>:6443/prometheus` (or start the
API server with `--prometheus-listen 0.0.0.0:9090`). We checked it with
Grafana 12.2: health check, range queries, metric browser and label
values. Dashboards built on raw cAdvisor, kube-state-metrics and
node-exporter series work. The kubernetes-mixin dashboards that ship with
kube-prometheus-stack do not, because they read recording rules litek8s
does not have.

If you would rather not run Grafana at all, `litek8s ui` has an Observe
view: active alerts, pods by CPU or memory against their limits with 30
minutes of sparklines, and log search across pods. It idles at 4.4 MiB RSS.

## What is not equal

This is the part to read before quoting the first table.

- **Series: 342 against 69,672.** kube-prometheus-stack keeps every
  cAdvisor and kubelet metric, kube-apiserver's per-resource histograms,
  per-CPU node metrics, and its own components'. litek8s keeps a curated
  set that covers the usual Kubernetes dashboard panels and its built-in
  alerts, not everything Prometheus can answer. It does sample three times
  more often (10 s against 30 s).
- **No scraping of your applications yet.** Prometheus scrapes any pod's
  `/metrics`; litek8s does not, neither by annotation nor by ServiceMonitor.
  The `AlertRule` above needs `http_errors_total` to come from somewhere:
  today, that is a Prometheus you point at your app.
- **The PromQL is a subset.** Selectors, `rate`/`irate`/`increase`, the
  `*_over_time` family, aggregations with `by`/`without`, binary operators
  with vector matching, `histogram_quantile`, `label_replace` and most
  functions are there. Subqueries (`x[1h:5m]`), the `@` modifier, native
  histograms, recording rules and exemplars are not.
- **Old data is coarser.** After 6 hours you get one point a minute, after
  7 days one every 10 minutes: counters keep their last value per bucket,
  gauges the mean. So `max_over_time` over last month sees averages, not
  peaks.
- **Logs are a scan, not an index.** Each file is bisected to the start of
  the time range, but days of a chatty cluster's logs are read in full on
  every node (in parallel). Loki indexes labels and keeps compressed chunks
  for its whole retention, past pod deletions and node rotations.
- **Queries.** Instant queries were as fast as Prometheus' (0.2–0.4 ms
  against 0.3–0.5 ms); one-hour range queries slower (0.8–4.3 ms against
  0.4–1.5 ms), below what a dashboard notices. Log queries took 11–27 ms
  against Loki's 15–49 ms.
- **One server.** Today one API server evaluates alerts and stores metrics.

Two measurement notes. The litek8s figure is an A/B: two identical 2 vCPU /
2 GiB guests with observability on and off (API server at 32.0 against
30.2 MiB). And the k3s figure's 1,371 MiB is the stack's own pods
(1,057 MiB) plus what k3s-server and containerd grew serving it (+314 MiB).

## When you still want the full stack

Run kube-prometheus-stack (or a Prometheus next to litek8s) when:

- your applications expose `/metrics` and you want them in the same place;
- you depend on recording rules, subqueries, or the kubernetes-mixin
  dashboards as they ship;
- you need high-resolution history beyond 6 hours, or indexed log search
  over weeks;
- you run a replicated control plane and need alerting to survive losing
  the server that evaluates it.

They are not exclusive: `/metrics` stays as it was for a real Prometheus,
`/prometheus/federate` exports the store, and installing metrics-server
takes `metrics.k8s.io` over (uninstalling it gives the group back). If you
want none of it, `--no-observability` turns it all off.

For a home lab, an edge box, a dev cluster or a single small server, the
answer we wanted was the platform's own metrics, logs and alerts, without
a second system to run next to it. That is what this is.

## Try it

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up
litek8s alerts
kubectl top pods -A --containers
```

Everything above, with every series listed and every flag, is in
[Metrics, logs, alerts](/docs/observability/). What is still missing is in
[Limitations](/docs/limitations/).
