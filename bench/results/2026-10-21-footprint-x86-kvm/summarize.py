#!/usr/bin/env python3
"""Medians over the rounds of this run: python3 summarize.py <this dir>  (-> summary.txt)

Lanes: on = litek8s apiserver --runtime crun (observability on, the default),
off = the same with --no-observability, k3s = k3s minimal. Per lane and window
(idle0: no pods, idle5: 5 nginx), memory is PSS in MiB from the samples at the
start and end of each 300 s window (2 per round): cp = control plane (the
litek8s process / k3s-server), rt = container runtime (crun shims / containerd,
shims, pause), add = addon pods (CoreDNS), orch = cp + rt + add, wl = nginx,
avail = guest MemAvailable. cpu busy = whole guest, % of one vCPU over the
window; cp = the control plane's share. ttr = apply -> first Ready pod, s.
[min-max] across samples.
"""
import json, os, sys, statistics, glob
R = sys.argv[1]
lanes = {"on": "kubecrun", "off": "kubecrun", "k3s": "k3smin"}
rows = {}
def mem(f):
    d = json.load(open(f)); c = d["categories"]; g = lambda k: c.get(k, {}).get("pss_kib", 0) / 1024
    return {"cp": g("control-plane"), "rt": g("runtime"), "add": g("addons"), "orch": g("control-plane") + g("runtime") + g("addons"),
            "wl": g("workload"), "avail": d["system"]["mem_available_kib"] / 1024}
def cpu(f):
    d = json.load(open(f))
    return 100.0 * d["busy_cpu_s"] / d["window_s"], d["categories"].get("control-plane", {}).get("pct_one_core", 0)
out = {}
for lane, tag in lanes.items():
    for rd in sorted(glob.glob(f"{R}/r*-{lane}")):
        if not os.path.exists(f"{rd}/{tag}.idle5.mem-end.json"): continue
        for w in ("idle0", "idle5"):
            for m in ("mem", "mem-end"):
                out.setdefault((lane, w), {"mem": [], "cpu": [], "ttr": []})["mem"].append(mem(f"{rd}/{tag}.{w}.{m}.json"))
            out[(lane, w)]["cpu"].append(cpu(f"{rd}/{tag}.{w}.cpu.json"))
        t = json.load(open(f"{rd}/{tag}.ttr.json"))
        out[(lane, "idle0")]["ttr"] += [x.get("ttr_s", x.get("seconds", 0)) for x in t]
        out[(lane, "idle0")].setdefault("size", []).append(json.load(open(f"{rd}/{tag}.size.json")))
med = statistics.median
for (lane, w), v in sorted(out.items()):
    ms = v["mem"]
    print(f"{lane:4} {w}: n={len(ms)} cp {med([m['cp'] for m in ms]):.1f} [{min(m['cp'] for m in ms):.1f}-{max(m['cp'] for m in ms):.1f}]"
          f" rt {med([m['rt'] for m in ms]):.1f} add {med([m['add'] for m in ms]):.1f} orch {med([m['orch'] for m in ms]):.1f}"
          f" [{min(m['orch'] for m in ms):.1f}-{max(m['orch'] for m in ms):.1f}] wl {med([m['wl'] for m in ms]):.1f} avail {med([m['avail'] for m in ms]):.0f}"
          f" | cpu busy {med([c[0] for c in v['cpu']]):.3f} [{min(c[0] for c in v['cpu']):.3f}-{max(c[0] for c in v['cpu']):.3f}] cp {med([c[1] for c in v['cpu']]):.3f}"
          + (f" | ttr {med(v['ttr']):.3f} n={len(v['ttr'])}" if v["ttr"] else ""))
