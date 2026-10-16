#!/usr/bin/env python3
"""In-guest measurement helper for bench/run.sh. Runs as root on the host under test.

  probe.py mem                         -> JSON: per-bucket RSS/PSS + system memory
  probe.py cpu <seconds>               -> JSON: per-bucket CPU over a sleep window
  probe.py cpu-snap <file>             -> save a CPU snapshot
  probe.py cpu-diff <file>             -> JSON: per-bucket CPU since the snapshot
  probe.py ttr-litek8s <sock> <yaml> <name>   -> JSON: apply -> first ready replica
  probe.py ttr-k3s <yaml> <name>              -> JSON: apply -> first Ready pod
  probe.py agent-direct <sock> <n> <json>     -> JSON: latency of n requests sent
                                                 straight to the resident agent socket
  probe.py ssh-rpc <n> <json> -- <ssh argv>   -> JSON: latency of n ssh-exec RPCs

Processes are bucketed by cgroup + comm so short-lived children (iptables,
ssh-spawned `litek8s agent` proxies) are still charged to the right owner via
the cgroup's cumulative cpu.stat even after they exit.
"""
import json
import os
import socket
import statistics
import struct
import subprocess
import sys
import time

CG = "/sys/fs/cgroup"
TICK = os.sysconf("SC_CLK_TCK")
SELF = {os.getpid(), os.getppid()}

CATEGORY = {
    "litek8s-serve": "control-plane",
    "k3s-server": "control-plane",
    "dockerd": "runtime",
    "docker-proxy": "runtime",
    "containerd": "runtime",
    "containerd-shim": "runtime",
    "k3s.service (short-lived)": "runtime",
    "docker.service (short-lived)": "runtime",
    "containerd.service (short-lived)": "runtime",
    "litek8s.service (short-lived)": "control-plane",
    "pod sandbox (pause)": "runtime",
    "k8s addon pods": "addons",
    "workload": "workload",
    "litek8s-agent": "node-agent",
    "sshd": "node-agent",
    "ssh-client": "node-agent",
    "user.slice (short-lived)": "node-agent",
    "ssh.service (short-lived)": "node-agent",
}


def read(path, default=""):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return default


def cgroup_of(pid):
    for line in read(f"/proc/{pid}/cgroup").splitlines():
        if line.startswith("0::"):
            return line[3:]
    return "/"


def comm_of(pid):
    return read(f"/proc/{pid}/comm").strip()


def anchor_of(cg):
    """The cgroup whose cumulative cpu.stat we charge: a pod, a system.slice unit,
    or user.slice as a whole."""
    parts = [p for p in cg.split("/") if p]
    if any("kubepods" in p for p in parts):
        for i, p in enumerate(parts):
            if p.startswith("pod") or "-pod" in p:
                return "/" + "/".join(parts[: i + 1])
        return "/" + "/".join(parts[:1])
    if parts[:1] == ["system.slice"] and len(parts) >= 2:
        return "/system.slice/" + parts[1]
    if parts[:1] == ["user.slice"]:
        return "/user.slice"
    return "/" + "/".join(parts[:1]) if parts else "/"


_pod_kind = {}


def pod_kind(anchor):
    if anchor not in _pod_kind:
        comms = set()
        for root, _dirs, files in os.walk(CG + anchor):
            if "cgroup.procs" in files:
                for pid in read(os.path.join(root, "cgroup.procs")).split():
                    comms.add(comm_of(pid))
        _pod_kind[anchor] = "workload" if "nginx" in comms else "k8s addon pods"
    return _pod_kind[anchor]


def bucket(cg, comm):
    a = anchor_of(cg)
    unit = a.rsplit("/", 1)[-1]
    if "kubepods" in a:
        if comm == "pause":
            return "pod sandbox (pause)"
        return pod_kind(a)
    if unit == "litek8s.service":
        return "litek8s-serve"
    if unit.startswith("docker-") and unit.endswith(".scope"):
        return "workload"
    if unit in ("docker.service", "containerd.service", "k3s.service"):
        if comm == "k3s-server":
            return "k3s-server"
        if comm.startswith("containerd-shim"):
            return "containerd-shim"
        if comm in ("dockerd", "docker-proxy", "containerd"):
            return comm
        return f"{unit} (short-lived)"
    if a == "/user.slice" or unit == "ssh.service":
        if comm == "litek8s":
            return "litek8s-agent"
        if comm in ("sshd", "sshd-session"):
            return "sshd"
        if comm == "ssh":
            return "ssh-client"
        return None  # our own shells / probe
    return None


def anchor_default_bucket(a):
    unit = a.rsplit("/", 1)[-1]
    if "kubepods" in a:
        # Only pod-level anchors; the kubepods root would double count every pod.
        return pod_kind(a) if (unit.startswith("pod") or "-pod" in unit) else None
    if unit.startswith("docker-") and unit.endswith(".scope"):
        return "workload"
    if unit in ("docker.service", "containerd.service", "k3s.service", "litek8s.service", "ssh.service"):
        return f"{unit} (short-lived)"
    if a == "/user.slice":
        return "user.slice (short-lived)"
    return None


def pids():
    for d in os.listdir("/proc"):
        if d.isdigit() and int(d) not in SELF:
            yield int(d)


# ---------------------------------------------------------------- memory


def mem():
    buckets = {}
    for pid in pids():
        status = read(f"/proc/{pid}/status")
        rss = next((int(l.split()[1]) for l in status.splitlines() if l.startswith("VmRSS:")), None)
        if rss is None:
            continue  # kernel thread
        pss = next(
            (int(l.split()[1]) for l in read(f"/proc/{pid}/smaps_rollup").splitlines() if l.startswith("Pss:")),
            0,
        )
        comm = comm_of(pid)
        b = bucket(cgroup_of(pid), comm) or "os/other"
        e = buckets.setdefault(b, {"rss_kib": 0, "pss_kib": 0, "procs": 0, "comms": {}})
        e["rss_kib"] += rss
        e["pss_kib"] += pss
        e["procs"] += 1
        e["comms"][comm] = e["comms"].get(comm, 0) + 1
    meminfo = {l.split(":")[0]: int(l.split()[1]) for l in read("/proc/meminfo").splitlines()}
    cg_mem = {}
    for unit in ("system.slice/litek8s.service", "system.slice/docker.service",
                 "system.slice/containerd.service", "system.slice/k3s.service", "kubepods", "kubepods.slice"):
        v = read(f"{CG}/{unit}/memory.current").strip()
        if v:
            cg_mem[unit] = int(v) // 1024
    return {
        "buckets": buckets,
        "categories": rollup(buckets, ("rss_kib", "pss_kib")),
        "system": {
            "mem_total_kib": meminfo["MemTotal"],
            "mem_available_kib": meminfo["MemAvailable"],
            "mem_used_kib": meminfo["MemTotal"] - meminfo["MemAvailable"],
        },
        "cgroup_memory_current_kib": cg_mem,
    }


def rollup(buckets, fields):
    out = {}
    for b, e in buckets.items():
        c = CATEGORY.get(b, "os/other")
        o = out.setdefault(c, {f: 0 for f in fields})
        for f in fields:
            o[f] += e[f]
    return out


# ---------------------------------------------------------------- cpu


def cpu_snapshot():
    procs = {}
    for pid in pids():
        stat = read(f"/proc/{pid}/stat")
        if not stat:
            continue
        rest = stat[stat.rfind(")") + 2:].split()
        ticks = int(rest[11]) + int(rest[12])  # utime + stime
        start = int(rest[19])
        io = read(f"/proc/{pid}/io")
        # wchar = bytes handed to write(); write_bytes = bytes that reached storage.
        wbytes = sum(int(l.split()[1]) for l in io.splitlines() if l.startswith("wchar:"))
        procs[pid] = (start, ticks, cgroup_of(pid), comm_of(pid), wbytes)
    anchors = {}
    for root, _dirs, files in os.walk(CG):
        if "cpu.stat" not in files:
            continue
        rel = root[len(CG):] or "/"
        if anchor_of(rel) != rel:
            continue
        line = next((l for l in read(root + "/cpu.stat").splitlines() if l.startswith("usage_usec")), None)
        if line:
            anchors[rel] = int(line.split()[1])
    cpu = read("/proc/stat").splitlines()[0].split()[1:]
    vals = [int(x) for x in cpu]
    busy = sum(vals) - vals[3] - vals[4]  # minus idle, iowait
    return {"t": time.time(), "procs": {str(k): v for k, v in procs.items()}, "anchors": anchors,
            "busy_ticks": busy, "total_ticks": sum(vals), "ncpu": os.cpu_count()}


def cpu_diff(a, b):
    secs = b["t"] - a["t"]
    buckets = {}
    charged = {}
    writes = {}

    def add(name, cpu_s):
        buckets[name] = buckets.get(name, 0.0) + cpu_s

    for pid, (start, ticks, cg, comm, wbytes) in b["procs"].items():
        prev = a["procs"].get(pid)
        if prev is None or prev[0] != start:
            delta, wdelta = ticks, wbytes  # started inside the window
        else:
            delta, wdelta = ticks - prev[1], wbytes - prev[4]
        bk = bucket(cg, comm)
        if bk and wdelta:
            writes[bk] = writes.get(bk, 0) + wdelta
        anc = anchor_of(cg)
        charged[anc] = charged.get(anc, 0.0) + delta / TICK
        if bk:
            add(bk, delta / TICK)
    for anc, usec in b["anchors"].items():
        default = anchor_default_bucket(anc)
        if not default:
            continue
        total = (usec - a["anchors"].get(anc, 0)) / 1e6
        residual = total - charged.get(anc, 0.0)
        if residual > 0.005:
            add(default, residual)
    busy = (b["busy_ticks"] - a["busy_ticks"]) / TICK
    attributed = sum(buckets.values())
    add("os/other", max(0.0, busy - attributed))
    to_pct = lambda s: round(100.0 * s / secs, 3)  # % of one core
    out_b = {k: {"cpu_s": round(v, 3), "pct_one_core": to_pct(v)} for k, v in sorted(buckets.items())}
    cats = {}
    for k, v in buckets.items():
        c = CATEGORY.get(k, "os/other")
        cats[c] = cats.get(c, 0.0) + v
    return {
        "window_s": round(secs, 1),
        "buckets": out_b,
        "categories": {k: {"cpu_s": round(v, 3), "pct_one_core": to_pct(v)} for k, v in sorted(cats.items())},
        "write_syscall_kib": {k: v // 1024 for k, v in sorted(writes.items())},
        "busy_cpu_s": round(busy, 3),
        "system_busy_pct_of_all_cpus": round(100.0 * (b["busy_ticks"] - a["busy_ticks"])
                                             / max(1, b["total_ticks"] - a["total_ticks"]), 3),
        "ncpu": b["ncpu"],
    }


# ---------------------------------------------------------------- litek8s client


def lk_request(sock_path, req):
    s = socket.socket(socket.AF_UNIX)
    s.connect(sock_path)
    body = json.dumps(req).encode()
    s.sendall(struct.pack(">I", len(body)) + body)
    hdr = b""
    while len(hdr) < 4:
        chunk = s.recv(4 - len(hdr))
        if not chunk:
            raise RuntimeError("daemon closed connection")
        hdr += chunk
    (n,) = struct.unpack(">I", hdr)
    buf = b""
    while len(buf) < n:
        buf += s.recv(n - len(buf))
    s.close()
    return json.loads(buf)


def lk_ready(sock_path, name):
    resp = lk_request(sock_path, "GetDeployments")
    for item in resp.get("Deployments", {}).get("items", []):
        if item["name"] == name:
            return item.get("ready_replicas", 0) >= 1
    return False


def ttr_litek8s(sock_path, manifest, name, timeout=180):
    t0 = time.monotonic()
    subprocess.run(["litek8s", "--socket-path", sock_path, "apply", "-f", manifest],
                   check=True, stdout=subprocess.DEVNULL)
    t_applied = time.monotonic()
    while time.monotonic() - t0 < timeout:
        if lk_ready(sock_path, name):
            t1 = time.monotonic()
            return {"seconds": round(t1 - t0, 3), "apply_cli_s": round(t_applied - t0, 3)}
        time.sleep(0.05)
    raise SystemExit(f"{name} not ready after {timeout}s")


def ttr_k3s(manifest, name, timeout=180):
    jsonpath = '{.metadata.name} {.status.conditions[?(@.type=="Ready")].status}{"\\n"}'
    watcher = subprocess.Popen(
        ["k3s", "kubectl", "get", "pods", "-l", f"app={name}", "-w", "-o", f"jsonpath={jsonpath}"],
        stdout=subprocess.PIPE, text=True)
    time.sleep(1.0)  # let the watch establish before the clock starts
    t0 = time.monotonic()
    subprocess.run(["k3s", "kubectl", "apply", "-f", manifest], check=True, stdout=subprocess.DEVNULL)
    t_applied = time.monotonic()
    try:
        for line in watcher.stdout:
            if line.strip().endswith("True"):
                t1 = time.monotonic()
                return {"seconds": round(t1 - t0, 3), "apply_cli_s": round(t_applied - t0, 3)}
            if time.monotonic() - t0 > timeout:
                break
    finally:
        watcher.kill()
    raise SystemExit(f"{name} not ready after {timeout}s")


# ---------------------------------------------------------------- rpc


def stats(samples_ms):
    s = sorted(samples_ms)
    return {"n": len(s), "mean_ms": round(statistics.mean(s), 3), "p50_ms": round(s[len(s) // 2], 3),
            "p95_ms": round(s[int(len(s) * 0.95) - 1], 3), "min_ms": round(s[0], 3)}


def agent_direct(sock_path, n, payload):
    samples = []
    data = payload.encode() + b"\n"
    for _ in range(n):
        t0 = time.perf_counter()
        s = socket.socket(socket.AF_UNIX)
        s.connect(sock_path)
        s.sendall(data)
        s.shutdown(socket.SHUT_WR)
        out = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            out += chunk
        s.close()
        samples.append((time.perf_counter() - t0) * 1000)
        json.loads(out)
    return stats(samples)


def ssh_rpc(n, payload, argv):
    samples = []
    data = payload.encode() + b"\n"
    snap = cpu_snapshot()
    for _ in range(n):
        t0 = time.perf_counter()
        p = subprocess.run(argv, input=data, capture_output=True)
        samples.append((time.perf_counter() - t0) * 1000)
        if p.returncode != 0:
            raise SystemExit(f"rpc failed: {p.stderr.decode()}")
    after = cpu_snapshot()
    out = stats(samples)
    out["caller_cpu_ms_per_rpc"] = round(
        1000.0 * (after["busy_ticks"] - snap["busy_ticks"]) / TICK / n, 3)
    return out


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "mem":
        out = mem()
    elif cmd == "cpu":
        a = cpu_snapshot()
        time.sleep(float(args[0]))
        out = cpu_diff(a, cpu_snapshot())
    elif cmd == "cpu-snap":
        with open(args[0], "w") as f:
            json.dump(cpu_snapshot(), f)
        return
    elif cmd == "cpu-diff":
        with open(args[0]) as f:
            out = cpu_diff(json.load(f), cpu_snapshot())
    elif cmd == "ttr-litek8s":
        out = ttr_litek8s(*args)
    elif cmd == "ttr-k3s":
        out = ttr_k3s(*args)
    elif cmd == "agent-direct":
        out = agent_direct(args[0], int(args[1]), args[2])
    elif cmd == "ssh-rpc":
        sep = args.index("--")
        out = ssh_rpc(int(args[0]), args[1], args[sep + 1:])
    else:
        raise SystemExit(__doc__)
    json.dump(out, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
