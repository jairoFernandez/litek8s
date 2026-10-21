#!/usr/bin/env python3
"""Memory of the scratch dev sandbox's systemd scope: cgroup totals per child and per-process PSS."""
import json, os, sys
home = os.path.expanduser("~/lk-mem-scratch/devhome")
pid = json.load(open(f"{home}/data/dev/env.json"))["pid"]
sc = "/sys/fs/cgroup" + open(f"/proc/{pid}/cgroup").read().split(":", 2)[2].strip()
sc = sc.rsplit("/", 1)[0] if sc.endswith("/supervisor") else sc
def stat(d):
    s = dict(l.split() for l in open(os.path.join(d, "memory.stat")))
    cur = int(open(os.path.join(d, "memory.current")).read())
    m = lambda b: round(b / 2**20, 1)
    return {"current": m(cur), "anon": m(int(s["anon"])), "file": m(int(s["file"])),
            "kernel": m(int(s["kernel"])), "in_use": m(cur - int(s["file"]))}
out = {"scope": stat(sc), "children": {}, "pss_kib": {}}
for root, dirs, files in os.walk(sc):
    rel = os.path.relpath(root, sc)
    if rel != "." and rel.count("/") < 3:
        out["children"][rel] = stat(root)
    for p in open(os.path.join(root, "cgroup.procs")).read().split():
        try:
            comm = open(f"/proc/{p}/comm").read().strip()
            pss = next(int(l.split()[1]) for l in open(f"/proc/{p}/smaps_rollup") if l.startswith("Pss:"))
        except Exception:
            continue
        if comm.startswith("litek8s"):
            args = open(f"/proc/{p}/cmdline").read().split("\0")
            comm = "litek8s " + " ".join(a for a in args[1:4] if a and not a.startswith("/"))
        out["pss_kib"][comm] = out["pss_kib"].get(comm, 0) + pss
out["pss_total_mib"] = round(sum(out["pss_kib"].values()) / 1024, 1)
print(json.dumps(out, indent=1 if "-v" in sys.argv else None))
