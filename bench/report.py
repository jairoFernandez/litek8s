#!/usr/bin/env python3
"""Render a bench/run.sh results directory as Markdown tables.

  python3 bench/report.py bench/results/<timestamp>
"""
import json
import os
import statistics
import sys
from collections import Counter

DIR = sys.argv[1]
SYSTEMS = [("litek8s", "litek8s"), ("kube", "litek8s apiserver + agent"), ("k3s", "k3s (default)"), ("k3smin", "k3s (minimal)")]


def load(name):
    path = os.path.join(DIR, name)
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return json.load(f)


def mib(kib):
    return f"{kib / 1024:.1f}"


def cat(mem, key, field="pss_kib"):
    return mem["categories"].get(key, {}).get(field, 0)


def pct(cpu, key):
    return cpu["categories"].get(key, {}).get("pct_one_core", 0.0)


def busy_pct(cpu):
    if "busy_cpu_s" not in cpu:
        return cpu["system_busy_pct_of_all_cpus"] * cpu["ncpu"]
    return 100.0 * cpu["busy_cpu_s"] / cpu["window_s"]


# Short-lived processes are charged via their cgroup; in the remote-node setup
# these residuals are the per-RPC processes.
RENAME = {
    "user.slice (short-lived)": "sshd-session + `litek8s agent` execs",
    "litek8s.service (short-lived)": "`ssh` clients forked by litek8s serve",
}

present = [(tag, label) for tag, label in SYSTEMS if load(f"{tag}.idle0.mem.json")]
out = []
p = out.append
env = load("env.json") or {}
p(f"Run `{os.path.basename(DIR.rstrip('/'))}` — {env.get('vm_cpus')} vCPU / {env.get('vm_mem_mib')} MiB guest "
  f"on {env.get('host')}, idle window {env.get('idle_secs')}s, litek8s `{env.get('litek8s_commit')}`.\n")

if present:
    p("### Idle memory (PSS MiB; RSS in parentheses)\n")
    p("| system | workloads | control plane | container runtime | addon pods | orchestrator total | nginx pods | guest used Δ vs boot |")
    p("|---|---|---|---|---|---|---|---|")
    for tag, label in present:
        base = load(f"{tag}.baseline.mem.json")["system"]["mem_used_kib"]
        for n, f in (("0", "idle0"), ("5", "idle5")):
            m = load(f"{tag}.{f}.mem.json")
            if not m:
                continue
            cells = []
            total_pss = total_rss = 0
            for c in ("control-plane", "runtime", "addons"):
                cells.append(f"{mib(cat(m, c))} ({mib(cat(m, c, 'rss_kib'))})" if cat(m, c, "rss_kib") else "–")
                total_pss += cat(m, c)
                total_rss += cat(m, c, "rss_kib")
            work = f"{mib(cat(m, 'workload'))}" if cat(m, "workload", "rss_kib") else "–"
            delta = m["system"]["mem_used_kib"] - base
            p(f"| {label} | {n} | {' | '.join(cells)} | **{mib(total_pss)}** ({mib(total_rss)}) | {work} | {mib(delta)} |")
    p("")

    p("### Idle CPU (% of one vCPU, averaged over the window)\n")
    p("| system | workloads | control plane | container runtime | addon pods | whole guest busy |")
    p("|---|---|---|---|---|---|")
    for tag, label in present:
        for n, f in (("0", "idle0"), ("5", "idle5")):
            c = load(f"{tag}.{f}.cpu.json")
            if not c:
                continue
            p(f"| {label} | {n} | {pct(c, 'control-plane'):.2f} | {pct(c, 'runtime'):.2f} | "
              f"{pct(c, 'addons'):.2f} | **{busy_pct(c):.2f}** |")
    p("")

    p("### Apply → first ready container (seconds, image pre-pulled)\n")
    p("| system | runs | median | min | max | of which CLI apply |")
    p("|---|---|---|---|---|---|")
    for tag, label in present:
        t = load(f"{tag}.ttr.json")
        if not t:
            continue
        s = [r["seconds"] for r in t]
        a = [r["apply_cli_s"] for r in t]
        p(f"| {label} | {len(s)} | **{statistics.median(s):.2f}** | {min(s):.2f} | {max(s):.2f} | {statistics.median(a):.2f} |")
    p("")

    p("### On-disk footprint (MiB)\n")
    p("| system | orchestrator binary | runtime / bundled data | disk used Δ after install (no workload images) |")
    p("|---|---|---|---|")
    for tag, label in present:
        z = load(f"{tag}.size.json")
        if not z:
            continue
        if tag in ("litek8s", "kube"):
            p(f"| {label} | {mib(z['litek8s_binary_kib'])} | docker.io {z['docker_io_version']} + deps: "
              f"{mib(z['pkg_installed_delta_kib'])} (dpkg Installed-Size) | **{mib(z['disk_used_delta_kib'])}** |")
        else:
            p(f"| {label} | {mib(z['k3s_binary_kib'])} | extracted data dir {mib(z['k3s_data_dir_kib'])} + "
              f"addon images {mib(z['k3s_containerd_store_kib'])} ({z['k3s_version']}) | **{mib(z['disk_used_delta_kib'])}** |")
    p("")

    p("### Control-plane writes during the 5-workload idle window\n")
    p("| system | write() KiB/min by control plane | state on disk (bytes) |")
    p("|---|---|---|")
    for tag, label in present:
        c = load(f"{tag}.idle5.cpu.json")
        if not c:
            continue
        w = sum(v for k, v in c.get("write_syscall_kib", {}).items() if k in ("litek8s-serve", "k3s-server"))
        state = open(os.path.join(DIR, f"{tag}.idle5.statefile-bytes.txt")).read().strip()
        p(f"| {label} | {w * 60 / c['window_s']:.0f} | {state} |")
    p("")

# ------------------------------------------------------------------ rpc
if load("rpc.ping.caller.json"):
    null = load("rpc.null.node-cpu.json")
    p("### litek8s remote-node RPC cost (control-plane VM → node VM over SSH ControlMaster)\n")
    p("| path | mean ms | p50 | p95 | caller CPU ms/RPC | node CPU ms/RPC |")
    p("|---|---|---|---|---|---|")
    rows = [("ssh mux, `true` (transport floor)", "ssh-true"),
            ("ssh mux + `litek8s agent` exec, Ping", "ping"),
            ("ssh mux + `litek8s agent` exec, ListContainersForDeployment", "list")]
    for label, key in rows:
        c = load(f"rpc.{key}.caller.json")
        n = load(f"rpc.{key}.node-cpu.json")
        busy_s = lambda x: busy_pct(x) * x["window_s"] / 100.0
        node_ms = 1000.0 * max(0.0, busy_s(n) - (busy_s(null) if null else 0)) / c["n"]
        p(f"| {label} | {c['mean_ms']:.2f} | {c['p50_ms']:.2f} | {c['p95_ms']:.2f} | "
          f"{c['caller_cpu_ms_per_rpc']:.1f} | {node_ms:.1f} |")
    for label, key in (("resident agent socket, Ping (no ssh/exec)", "direct-ping"),
                       ("resident agent socket, ListContainersForDeployment", "direct-list")):
        d = load(f"rpc.{key}.json")
        p(f"| {label} | {d['mean_ms']:.2f} | {d['p50_ms']:.2f} | {d['p95_ms']:.2f} | – | – |")
    p("")

if os.path.exists(os.path.join(DIR, "rpc.census.window")):
    window = float(open(os.path.join(DIR, "rpc.census.window")).read())
    kinds = Counter()
    times = []
    for line in open(os.path.join(DIR, "rpc.census.log")):
        ts, _, req = line.strip().partition(" ")
        try:
            obj = json.loads(req)
            kind = obj if isinstance(obj, str) else next(iter(obj))
        except json.JSONDecodeError:  # truncated by the shim's 160-char cut
            kind = req.lstrip('{"').split('"', 1)[0]
        kinds[kind or "(unparsed)"] += 1
        times.append(float(ts))
    total = sum(kinds.values())
    passes = kinds.get("ListManagedContainers", 0) or 1
    reconcile_kinds = ("ListManagedContainers", "ListContainersForDeployment", "ProbeContainerTcp",
                       "InspectExitState", "CreateAndStart", "RemoveContainer")
    per_pass = sum(kinds[k] for k in reconcile_kinds) / passes
    p(f"### RPC census: 5 nginx deployments on one SSH node, steady state, {window:.0f}s\n")
    p(f"{total} RPCs = **{total / window:.1f} RPC/s** (each one an `ssh` fork on the control plane and an "
      f"sshd session + `litek8s agent` exec on the node). Reconcile passes observed: {passes} "
      f"(≈ every {window / passes:.1f}s), **{per_pass:.1f} RPCs per reconcile pass**.\n")
    p("| request | count | per second | loop |")
    p("|---|---|---|---|")
    for k, v in kinds.most_common():
        loop = "reconcile" if k in reconcile_kinds else "cluster snapshot"
        p(f"| {k} | {v} | {v / window:.2f} | {loop} |")
    p("")

    nc, cc = load("rpc.node.idle5.cpu.json"), load("rpc.cp.idle5.cpu.json")
    if nc and cc:
        local = load("litek8s.idle5.cpu.json")
        p("### Steady-state CPU with those 5 deployments on the remote node (% of one vCPU)\n")
        p("| where | top consumers | whole guest busy |")
        p("|---|---|---|")
        for label, c in (("node VM", nc), ("control-plane VM", cc)):
            top = sorted(c["buckets"].items(), key=lambda kv: -kv[1]["pct_one_core"])[:5]
            p(f"| {label} | " + ", ".join(f"{RENAME.get(k, k)} {v['pct_one_core']:.2f}" for k, v in top)
              + f" | **{busy_pct(c):.2f}** |")
        if local:
            p(f"| (same 5 deployments on litek8s's local node, for reference) | | {busy_pct(local):.2f} |")
        p("")

print("\n".join(out))
