#!/usr/bin/env python3
"""In-guest driver of the observability benchmark (bench/run.sh obs-*). Runs as
root with KUBECONFIG set.

  obs.py endpoints <k3s|litek8s>           -> JSON: where to query metrics and logs
  obs.py storage <k3s|litek8s>             -> JSON: bytes on disk of the metric and log stores
  obs.py tsdb <k3s|litek8s>                -> JSON: series, samples appended (the store's own metrics)
  obs.py queries <k3s|litek8s> <runs>      -> JSON: latency of common metric and log queries
  obs.py loki-flush                        -> flush Loki's in-memory chunks to disk
"""
import glob
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.parse
import urllib.request


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True).stdout.strip()


def svc_ip(ns, name):
    return sh("kubectl", "-n", ns, "get", "svc", name, "-o", "jsonpath={.spec.clusterIP}")


def endpoints(system):
    if system == "litek8s":
        return {"prom": "http://127.0.0.1:6443/prometheus", "logs": "http://127.0.0.1:6443/litek8s/v1/logs"}
    prom = sh("kubectl", "-n", "monitoring", "get", "svc", "-l", "app=kube-prometheus-stack-prometheus", "-o", "jsonpath={.items[0].metadata.name}")
    return {"prom": f"http://{svc_ip('monitoring', prom)}:9090", "loki": f"http://{svc_ip('monitoring', 'loki')}:3100"}


def get(url, timeout=60):
    t0 = time.monotonic()
    with urllib.request.urlopen(url, timeout=timeout) as r:
        body = r.read()
    return time.monotonic() - t0, body


def du(paths):
    paths = [p for p in paths if os.path.exists(p)]
    if not paths:
        return 0
    out = sh("du", "-sk", "--total", *paths)
    return int(out.splitlines()[-1].split()[0]) * 1024 if out else 0


def storage(system):
    if system == "litek8s":
        base = "/var/lib/litek8s/apiserver"
        obs = f"{base}/obs"
        return {
            "metrics_bytes": du([obs]),
            "metrics_parts": {p: du([f"{obs}/{p}"]) for p in ("raw", "60s", "600s", "wal.log", "series.log")},
            "log_archive_bytes": du([f"{base}/node-agent/logs"]),
            "container_log_files_bytes": sum(os.path.getsize(f) for f in glob.glob(f"{base}/node-agent/crun/containers/*/log*")),
        }
    # Prometheus: an emptyDir of its pod (the chart's default); Loki: its PVC (local-path).
    prom = glob.glob("/var/lib/kubelet/pods/*/volumes/kubernetes.io~empty-dir/prometheus-kube-prometheus-stack-prometheus-db")
    parts = {}
    for p in prom:
        for sub in ("wal", "chunks_head"):
            parts[sub] = du([f"{p}/prometheus-db/{sub}"])
        parts["blocks"] = du(glob.glob(f"{p}/prometheus-db/01*"))
    loki = glob.glob("/var/lib/rancher/k3s/storage/*loki*")
    loki_parts = {}
    for p in loki:
        for sub in ("chunks", "wal", "tsdb-shipper-active", "tsdb-shipper-cache", "compactor"):
            loki_parts[sub] = loki_parts.get(sub, 0) + du(glob.glob(f"{p}/**/{sub}", recursive=True)[:1])
    return {
        "metrics_bytes": du(prom),
        "metrics_parts": parts,
        "logs_bytes": du(loki),
        "logs_parts": loki_parts,
        "container_log_files_bytes": sum(os.path.getsize(f) for f in glob.glob("/var/log/pods/*/*/*.log*")),
    }


def prom_query(base, q):
    _, body = get(f"{base}/api/v1/query?" + urllib.parse.urlencode({"query": q}))
    r = json.loads(body)["data"]["result"]
    return float(r[0]["value"][1]) if r else None


def tsdb(system):
    base = endpoints(system)["prom"]
    if system == "litek8s":
        return {
            "series": prom_query(base, "litek8s_tsdb_series"),
            "samples_appended": prom_query(base, "litek8s_tsdb_samples_appended_total"),
            "head_bytes": prom_query(base, "litek8s_tsdb_head_bytes"),
            "index_bytes": prom_query(base, "litek8s_tsdb_index_bytes"),
            "disk_bytes": prom_query(base, "litek8s_tsdb_disk_bytes"),
        }
    return {
        "series": prom_query(base, "prometheus_tsdb_head_series"),
        "samples_appended": prom_query(base, "sum(prometheus_tsdb_head_samples_appended_total)"),
        "targets_up": prom_query(base, "count(up == 1)"),
        "targets": prom_query(base, "count(up)"),
    }


# Queries a Grafana Kubernetes dashboard and an alert rule would send, with
# the names both systems serve.
METRIC_QUERIES = [
    ("cpu by namespace", 'sum by (namespace) (rate(container_cpu_usage_seconds_total{container!=""}[5m]))'),
    ("top 5 pods by memory", 'topk(5, sum by (namespace, pod) (container_memory_working_set_bytes{container!=""}))'),
    ("pods by phase", "sum by (phase) (kube_pod_status_phase)"),
    ("restarts in 1h", "sum by (namespace, pod) (increase(kube_pod_container_status_restarts_total[1h]))"),
    ("node memory available", "node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes"),
    ("node cpu busy", 'sum(rate(node_cpu_seconds_total{mode!="idle"}[5m]))'),
    ("cpu vs requests", 'sum by (pod) (rate(container_cpu_usage_seconds_total{namespace="bench",container!=""}[5m])) / sum by (pod) (kube_pod_container_resource_requests{namespace="bench",resource="cpu"})'),
]


def timed(fn, runs):
    times = []
    out = None
    for _ in range(runs):
        t, out = fn()
        times.append(t)
    times.sort()
    return {"median_ms": round(1000 * statistics.median(times), 2), "p90_ms": round(1000 * times[int(0.9 * (len(times) - 1))], 2), "runs": runs}, out


def queries(system, runs):
    ep = endpoints(system)
    now = time.time()
    out = {"instant": {}, "range_1h": {}, "logs": {}}
    for name, q in METRIC_QUERIES:
        stats, body = timed(lambda: get(f"{ep['prom']}/api/v1/query?" + urllib.parse.urlencode({"query": q, "time": now})), runs)
        stats["series"] = len(json.loads(body)["data"]["result"])
        out["instant"][name] = stats
        args = {"query": q, "start": now - 3600, "end": now, "step": 15}
        stats, body = timed(lambda: get(f"{ep['prom']}/api/v1/query_range?" + urllib.parse.urlencode(args)), runs)
        res = json.loads(body)["data"]["result"]
        stats["series"] = len(res)
        stats["points"] = sum(len(r["values"]) for r in res)
        out["range_1h"][name] = stats
    # Logs: the errors of one app over the last hour, and a substring search
    # across every pod over 15 minutes.
    log_queries = [
        ("errors of app=loggen, 1h", {"selector": "app=loggen", "grep": "ERROR", "since": "1h", "limit": "1000"},
         '{namespace="bench", app="loggen"} |= "ERROR"', 3600),
        ("'timeout' in every pod, 15m", {"grep": "timeout", "since": "15m", "limit": "1000"}, '{namespace=~".+"} |= "timeout"', 900),
    ]
    for name, lk, logql, secs in log_queries:
        if system == "litek8s":
            url = f"{ep['logs']}?" + urllib.parse.urlencode(lk)
            stats, body = timed(lambda: get(url), runs)
            stats["lines"] = len([l for l in body.splitlines() if l.strip() and b'"error"' not in l[:20]])
        else:
            args = {"query": logql, "start": int((now - secs) * 1e9), "end": int(now * 1e9), "limit": 1000, "direction": "backward"}
            url = f"{ep['loki']}/loki/api/v1/query_range?" + urllib.parse.urlencode(args)
            stats, body = timed(lambda: get(url), runs)
            stats["lines"] = sum(len(s["values"]) for s in json.loads(body)["data"]["result"])
        out["logs"][name] = stats
    return out


def loki_flush():
    ep = endpoints("k3s")
    req = urllib.request.Request(f"{ep['loki']}/flush", method="POST")
    with urllib.request.urlopen(req, timeout=60) as r:
        return {"status": r.status}


def main():
    cmd = sys.argv[1]
    if cmd == "endpoints":
        print(json.dumps(endpoints(sys.argv[2])))
    elif cmd == "storage":
        print(json.dumps(storage(sys.argv[2])))
    elif cmd == "tsdb":
        print(json.dumps(tsdb(sys.argv[2])))
    elif cmd == "queries":
        print(json.dumps(queries(sys.argv[2], int(sys.argv[3])), indent=1))
    elif cmd == "loki-flush":
        print(json.dumps(loki_flush()))
    else:
        sys.exit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
