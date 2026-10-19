#!/usr/bin/env python3
"""Write probe for the HA benchmark (bench/HA.md): PUT one ConfigMap over and
over through the kubeconfig's server, on one keep-alive connection (as
client-go does), reconnecting when it breaks. Works against litek8s (plain
HTTP) and k3s (TLS, client certificate).

  haprobe.py run <kubeconfig> <seconds> <interval-ms> <out.jsonl>
  haprobe.py report <out.jsonl> [<nemesis.log>]

`run` writes one JSON line per write: {"t": start (epoch s), "ms": latency,
"ok": bool}. `report` prints latency percentiles of the successful writes and
the windows without a successful write longer than 500 ms (with the kill
that preceded each, from bench/ha.sh's nemesis log).
"""
import base64
import http.client
import json
import os
import ssl
import sys
import tempfile
import time
import urllib.parse


def load_kubeconfig(path):
    # Tiny YAML subset: the files litek8s and k3s write.
    text = open(path).read()
    vals = {}
    for line in text.splitlines():
        s = line.strip().lstrip("- ")
        for key in ("server", "certificate-authority-data", "client-certificate-data", "client-key-data"):
            if s.startswith(key + ":"):
                vals.setdefault(key, s.split(":", 1)[1].strip())
    return vals


class Client:
    def __init__(self, kubeconfig):
        cfg = load_kubeconfig(kubeconfig)
        url = urllib.parse.urlparse(cfg["server"])
        self.host, self.port, self.https = url.hostname, url.port or (443 if url.scheme == "https" else 80), url.scheme == "https"
        self.ctx = None
        if self.https:
            tmp = tempfile.mkdtemp()
            ctx = ssl.create_default_context()
            if "certificate-authority-data" in cfg:
                ca = os.path.join(tmp, "ca.crt")
                open(ca, "wb").write(base64.b64decode(cfg["certificate-authority-data"]))
                ctx.load_verify_locations(ca)
            if "client-certificate-data" in cfg:
                crt, key = os.path.join(tmp, "c.crt"), os.path.join(tmp, "c.key")
                open(crt, "wb").write(base64.b64decode(cfg["client-certificate-data"]))
                open(key, "wb").write(base64.b64decode(cfg["client-key-data"]))
                ctx.load_cert_chain(crt, key)
            self.ctx = ctx
        self.conn = None

    def request(self, method, path, body=None, timeout=10):
        if self.conn is None:
            if self.https:
                self.conn = http.client.HTTPSConnection(self.host, self.port, timeout=timeout, context=self.ctx)
            else:
                self.conn = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        try:
            data = json.dumps(body).encode() if body is not None else None
            self.conn.request(method, path, body=data, headers={"Content-Type": "application/json", "Accept": "application/json"})
            resp = self.conn.getresponse()
            payload = resp.read()
            return resp.status, payload
        except Exception:
            try:
                self.conn.close()
            finally:
                self.conn = None
            raise


def run(kubeconfig, seconds, interval_ms, out):
    c = Client(kubeconfig)
    name = "haprobe"
    base = "/api/v1/namespaces/default/configmaps"
    cm = lambda n: {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}, "data": {"n": str(n)}}
    try:
        c.request("POST", base, cm(0))
    except Exception:
        pass
    end = time.time() + seconds
    n = 0
    with open(out, "w") as f:
        while time.time() < end:
            n += 1
            t0 = time.time()
            ok = False
            try:
                status, _ = c.request("PUT", f"{base}/{name}", cm(n))
                ok = status == 200
                if status == 404:
                    c.request("POST", base, cm(n))
            except Exception:
                ok = False
            ms = (time.time() - t0) * 1e3
            f.write(json.dumps({"t": t0, "ms": round(ms, 3), "ok": ok}) + "\n")
            if not ok:
                time.sleep(0.02)
            wait = interval_ms / 1e3 - (time.time() - t0)
            if wait > 0:
                time.sleep(wait)


def report(out, nemesis=None):
    samples = [json.loads(l) for l in open(out)]
    lat = sorted(s["ms"] for s in samples if s["ok"])
    pct = lambda p: lat[min(len(lat) - 1, int(round((len(lat) - 1) * p)))] if lat else float("nan")
    kills = []
    if nemesis and os.path.exists(nemesis):
        for line in open(nemesis):
            parts = line.split()
            if len(parts) >= 3 and parts[1] == "kill":
                kills.append((float(parts[0]), parts[2]))
    windows = []
    last = None
    for s in samples:
        if s["ok"]:
            end = s["t"] + s["ms"] / 1e3
            if last is not None and end - last > 0.5:
                windows.append((last, end))
            last = end
    res = {
        "writes": len(samples),
        "ok": len(lat),
        "failed": len(samples) - len(lat),
        "p50_ms": pct(0.5), "p90_ms": pct(0.9), "p99_ms": pct(0.99), "max_ms": lat[-1] if lat else None,
        "windows": [],
    }
    for a, b in windows:
        k = [x for x in kills if a - 1 <= x[0] <= b]
        res["windows"].append({"seconds": round(b - a, 3), "after_kill_s": round(b - k[-1][0], 3) if k else None, "killed": k[-1][1] if k else None})
    if kills:
        # Kills that caused no window are zero-downtime failovers.
        res["kills"] = len(kills)
    print(json.dumps(res, indent=1))


if __name__ == "__main__":
    if sys.argv[1] == "run":
        run(sys.argv[2], float(sys.argv[3]), float(sys.argv[4]), sys.argv[5])
    else:
        report(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
