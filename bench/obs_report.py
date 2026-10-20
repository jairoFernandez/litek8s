#!/usr/bin/env python3
"""Tables for the observability phases of bench/run.sh.

  bench/obs_report.py <results dir with obs-k3s.* and obs-litek8s.* files>
"""
import json
import os
import sys

D = sys.argv[1]
SYS = [("k3s", "k3s minimal + kube-prometheus-stack + Loki + Alloy"), ("litek8s", "litek8s (built in)")]


def load(name):
    p = os.path.join(D, name)
    if not os.path.exists(p):
        return None
    with open(p) as f:
        text = f.read()
    try:
        return json.loads(text)
    except ValueError:
        # Several JSON documents (a command's output, then the measurement): the last.
        return json.loads(text.strip().splitlines()[-1])


def mib(kib):
    return f"{kib / 1024:,.0f}"


def cat(mem, c):
    return mem["categories"].get(c, {}).get("pss_kib", 0) if mem else 0


def cpu(c, name):
    return c["categories"].get(name, {}).get("pct_one_core", 0.0) if c else 0.0


def used(mem):
    return mem["system"]["mem_used_kib"] if mem else 0


def human(b):
    for u in ["B", "KiB", "MiB", "GiB", "TiB"]:
        if abs(b) < 1024:
            return f"{b:,.1f} {u}" if u != "B" else f"{b:,.0f} B"
        b /= 1024
    return f"{b:,.1f} PiB"


rows = {}
for s, _ in SYS:
    t = f"obs-{s}"
    r = {
        "mem_noobs": load(f"{t}.noobs.mem1.json"),
        "mem_obs": load(f"{t}.obs.mem1.json"),
        "cpu_noobs": load(f"{t}.noobs.cpu.json"),
        "cpu_obs": load(f"{t}.obs.cpu.json"),
        "st0": load(f"{t}.obs.storage0.json"),
        "st1": load(f"{t}.obs.storage1.json"),
        "stf": load(f"{t}.obs.storage-flushed.json"),
        "ts0": load(f"{t}.obs.tsdb0.json"),
        "ts1": load(f"{t}.obs.tsdb1.json"),
        "disk": load(f"{t}.disk.json"),
        "install": load(f"{t}.install.json"),
        "q": load(f"{t}.queries.json"),
        "info": load(f"{t}.info.json"),
    }
    rows[s] = r

window = (rows["litek8s"]["cpu_obs"] or rows["k3s"]["cpu_obs"] or {}).get("window_s", 3600)
out = []
out.append(f"Observability on k3s vs built into litek8s, measured window {window / 60:.0f} min after a warm-up, same workload (bench/obs/workload.yaml).\n")
out.append("### Memory, PSS MiB (end of the window)\n")
out.append("| | " + " | ".join(n for _, n in SYS) + " |")
out.append("|---|" + "---|" * len(SYS))


def line(label, f):
    out.append(f"| {label} | " + " | ".join(f(rows[s]) for s, _ in SYS) + " |")


orch = lambda m: cat(m, "control-plane") + cat(m, "runtime") + cat(m, "addons")
line("Orchestrator, workload only (no observability)", lambda r: mib(orch(r["mem_noobs"])))
line("Orchestrator, with observability", lambda r: mib(orch(r["mem_obs"])))
line("Observability stack's pods", lambda r: mib(cat(r["mem_obs"], "observability")))
line("**Observability's cost** (orchestrator growth + its pods)", lambda r: "**" + mib(orch(r["mem_obs"]) - orch(r["mem_noobs"]) + cat(r["mem_obs"], "observability")) + "**")
line("Guest memory used (MemTotal − MemAvailable), before → after", lambda r: f"{mib(used(r['mem_noobs']))} → {mib(used(r['mem_obs']))}")
out.append("")
out.append("### CPU, % of one vCPU (average over the window)\n")
out.append("| | " + " | ".join(n for _, n in SYS) + " |")
out.append("|---|" + "---|" * len(SYS))
line("Control plane, workload only", lambda r: f"{cpu(r['cpu_noobs'], 'control-plane'):.2f}")
line("Control plane, with observability", lambda r: f"{cpu(r['cpu_obs'], 'control-plane'):.2f}")
line("Observability stack's pods", lambda r: f"{cpu(r['cpu_obs'], 'observability'):.2f}")
line("Runtime (containerd/shims, crun shims)", lambda r: f"{cpu(r['cpu_obs'], 'runtime'):.2f}")
line(
    "**Observability's cost**",
    lambda r: f"**{cpu(r['cpu_obs'], 'control-plane') - cpu(r['cpu_noobs'], 'control-plane') + cpu(r['cpu_obs'], 'observability') + cpu(r['cpu_obs'], 'runtime') - cpu(r['cpu_noobs'], 'runtime'):.2f}**",
)
line("Whole guest busy, % of all vCPUs", lambda r: f"{r['cpu_obs']['system_busy_pct_of_all_cpus']:.2f}" if r["cpu_obs"] else "-")
out.append("")
out.append("### Disk\n")
out.append("| | " + " | ".join(n for _, n in SYS) + " |")
out.append("|---|" + "---|" * len(SYS))
line("Installed (images, binaries)", lambda r: human((r["disk"]["with_observability_kib"] - r["disk"]["with_workload_kib"]) * 1024) if r["disk"] else "-")


def rate(r):
    if not (r["ts0"] and r["ts1"]):
        return None
    return (r["ts1"]["samples_appended"] - r["ts0"]["samples_appended"]) / window


line("Series", lambda r: f"{r['ts1']['series']:,.0f}" if r["ts1"] else "-")
line("Samples ingested / s", lambda r: f"{rate(r):,.1f}" if rate(r) else "-")


def metric_growth(r):
    return (r["st1"]["metrics_bytes"] - r["st0"]["metrics_bytes"]) if r["st0"] and r["st1"] else 0


def compressed_growth(r):
    # Prometheus: the head chunks on disk (the compressed samples; the WAL
    # holds them uncompressed until a block is cut). litek8s: its blocks.
    if not (r["st0"] and r["st1"]):
        return 0
    p0, p1 = r["st0"]["metrics_parts"], r["st1"]["metrics_parts"]
    keys = ["chunks_head", "blocks"] if "chunks_head" in p1 else ["raw", "60s", "600s"]
    return sum(p1.get(k, 0) - p0.get(k, 0) for k in keys)


line("Metric store growth over the window (all files)", lambda r: human(metric_growth(r)))
line("… compressed samples only (Prometheus: chunks_head; litek8s: blocks)", lambda r: human(compressed_growth(r)))
line("Bytes per sample (compressed)", lambda r: f"{compressed_growth(r) / (rate(r) * window):.2f}" if rate(r) else "-")


def logs_growth(r):
    if r["stf"] and r["st0"]:
        return r["stf"]["logs_bytes"] - r["st0"].get("logs_bytes", 0)
    if r["st0"] and r["st1"]:
        return r["st1"].get("logs_bytes", r["st1"].get("log_archive_bytes", 0)) - r["st0"].get("logs_bytes", r["st0"].get("log_archive_bytes", 0))
    return 0


line("Log store growth over the window (Loki after a flush; litek8s: its archive)", lambda r: human(logs_growth(r)))
line("Container log files (CRI, both systems keep them)", lambda r: human(r["st1"]["container_log_files_bytes"]) if r["st1"] else "-")
per_day = 86400 / window
line("Metrics per day of retention, compressed", lambda r: human(compressed_growth(r) * per_day))
line("Logs per day of retention", lambda r: human(logs_growth(r) * per_day))
out.append("")
out.append("### Queries, median ms (p90), 20 runs each\n")
out.append("| query | " + " | ".join(n for _, n in SYS) + " |")
out.append("|---|" + "---|" * len(SYS))
names = list((rows["litek8s"]["q"] or rows["k3s"]["q"] or {}).get("instant", {}).keys())
for kind, label in [("instant", "instant"), ("range_1h", "1 h range, 15 s step")]:
    for n in names:
        cells = []
        for s, _ in SYS:
            q = (rows[s]["q"] or {}).get(kind, {}).get(n)
            cells.append(f"{q['median_ms']:.1f} ({q['p90_ms']:.1f}), {q['series']} series" if q else "-")
        out.append(f"| {n} ({label}) | " + " | ".join(cells) + " |")
for n in (rows["litek8s"]["q"] or rows["k3s"]["q"] or {}).get("logs", {}).keys():
    cells = []
    for s, _ in SYS:
        q = (rows[s]["q"] or {}).get("logs", {}).get(n)
        cells.append(f"{q['median_ms']:.1f} ({q['p90_ms']:.1f}), {q['lines']} lines" if q else "-")
    out.append(f"| logs: {n} | " + " | ".join(cells) + " |")
out.append("")
for s, n in SYS:
    info = rows[s]["info"]
    if info:
        restarts = sum(p["restarts"] for p in info["pods"])
        out.append(f"- {n}: {info.get('server_version')}, {info.get('k3s_version') or info.get('crun_version')}; {len(info['pods'])} pods, {restarts} restarts")
        if info.get("charts"):
            try:
                charts = json.loads(info["charts"])
                out.append("  charts: " + ", ".join(f"{c['chart']} ({c['app_version']})" for c in charts))
            except (ValueError, KeyError):
                pass
print("\n".join(out))
