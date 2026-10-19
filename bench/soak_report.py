#!/usr/bin/env python3
"""Summarize a bench/soak.sh run: trends per process and per store metric.

    bench/soak_report.py bench/results/<date>-soak [--markdown]

For each series it prints the first, last, min and max value and the
least-squares slope per hour over the run's second half (after warm-up), so
a leak shows as a steady positive slope and a bounded series as a slope near
zero. CPU is the average share of one core between samples.
"""
import json
import sys


def load(path):
    rows = []
    with open(path) as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return rows


def slope_per_hour(points):
    """Least-squares slope of (t seconds, v) points, per hour."""
    n = len(points)
    if n < 3:
        return 0.0
    mt = sum(t for t, _ in points) / n
    mv = sum(v for _, v in points) / n
    den = sum((t - mt) ** 2 for t, _ in points)
    if den == 0:
        return 0.0
    return sum((t - mt) * (v - mv) for t, v in points) / den * 3600


def series(rows):
    out = {}

    def add(name, t, v):
        if v is not None:
            out.setdefault(name, []).append((t, float(v)))

    prev = {}
    for r in rows:
        t = r["t"]
        for node, s in (r.get("nodes") or {}).items():
            if not s:
                continue
            add(f"{node} MemAvailable MiB", t, s.get("mem_available_kib", 0) / 1024)
            for role, p in (s.get("procs") or {}).items():
                key = f"{node} {role}"
                add(f"{key} PSS MiB", t, p["pss_kib"] / 1024)
                add(f"{key} count", t, p["count"])
                add(f"{key} fds", t, p["fds"])
                add(f"{key} threads", t, p["threads"])
                # CPU only between samples of the same set of processes.
                last = prev.get(key)
                if last and last[2] == p["count"] and p["cpu_s"] >= last[1] and t > last[0]:
                    add(f"{key} CPU %", t, (p["cpu_s"] - last[1]) / (t - last[0]) * 100)
                prev[key] = (t, p["cpu_s"], p["count"])
        m = r.get("metrics") or {}
        for name in ("litek8s_store_objects", "litek8s_store_wal_bytes", "litek8s_store_wal_records",
                     "litek8s_store_compactions_total", "litek8s_store_history_bytes", "litek8s_apiserver_watches",
                     "litek8s_store_feed_receivers", "litek8s_agent_connections", "litek8s_agent_streams",
                     "litek8s_process_resident_bytes"):
            if name in m:
                v = m[name]
                if name.endswith("_bytes"):
                    v = v / (1 << 20)
                    name = name[:-6] + "_MiB"
                add(name, t, v)
        # Mean request latency per verb over the interval.
        for verb in ("get", "list", "create", "update", "patch", "delete"):
            s_key = f'litek8s_apiserver_request_duration_seconds_sum{{verb="{verb}"}}'
            c_key = f'litek8s_apiserver_request_duration_seconds_count{{verb="{verb}"}}'
            if s_key in m and c_key in m:
                last = prev.get(("lat", verb))
                if last and m[c_key] > last[1]:
                    add(f"API {verb} mean ms", t, (m[s_key] - last[0]) / (m[c_key] - last[1]) * 1000)
                prev[("lat", verb)] = (m[s_key], m[c_key])
        for path, secs in (r.get("latency") or {}).items():
            add(f"GET {path} ms", t, secs * 1000)
        c = r.get("cluster") or {}
        add("pods (all namespaces)", t, c.get("pods"))
        add("container restarts", t, c.get("restarts"))
        p = r.get("prober") or {}
        if p.get("ok", 0) + p.get("fail", 0):
            add("prober success %", t, p["ok"] / (p["ok"] + p["fail"]) * 100)
    return out


def main():
    run = sys.argv[1]
    rows = load(f"{run}/samples.jsonl")
    if not rows:
        sys.exit("no samples")
    hours = (rows[-1]["t"] - rows[0]["t"]) / 3600
    data = series(rows)
    print(f"{len(rows)} samples over {hours:.1f} h\n")
    print("| series | first | last | min | max | slope/h (2nd half) |")
    print("|---|---:|---:|---:|---:|---:|")
    for name in sorted(data):
        pts = data[name]
        vals = [v for _, v in pts]
        half = pts[len(pts) // 2:]
        print(f"| {name} | {vals[0]:.2f} | {vals[-1]:.2f} | {min(vals):.2f} | {max(vals):.2f} | {slope_per_hour(half):+.3f} |")
    fails = [(r["t"], r["prober"]) for r in rows if (r.get("prober") or {}).get("fail")]
    total_ok = sum((r.get("prober") or {}).get("ok", 0) for r in rows)
    total_fail = sum((r.get("prober") or {}).get("fail", 0) for r in rows)
    if total_ok + total_fail:
        print(f"\nprober: {total_ok} ok, {total_fail} failed ({total_fail / (total_ok + total_fail) * 100:.3f} %) in {len(fails)} sample(s) with failures")


if __name__ == "__main__":
    main()
