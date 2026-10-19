#!/usr/bin/env python3
"""In-guest Argo CD driver for the `argocd` phase of bench/run.sh. Uses kubectl
and helm from PATH against $KUBECONFIG, so it runs unchanged on k3s and litek8s.

  argocd.py install <chart.tgz> <values.yaml>  -> JSON: helm install -> every pod Ready
  argocd.py uninstall                          -> helm uninstall, wait for the pods to go
  argocd.py app-create <app.yaml> <name>       -> JSON: apply an Application -> Synced+Healthy
  argocd.py app-delete <name>                  -> delete it (cascade), wait for its resources
  argocd.py app-scale <name> <replicas>        -> JSON: helm parameter override -> re-synced,
                                                  Deployment at that many ready replicas
  argocd.py drift <name> <ns> <deployment>     -> JSON: delete a managed Deployment -> back
                                                  and Available, app Synced+Healthy
  argocd.py metrics                            -> JSON: apiserver_request_total and
                                                  apiserver_longrunning_requests from /metrics,
                                                  plus this driver's request counter

Times are wall clock from the first client call to the first poll (every
POLL seconds) that sees the end state.
"""
import json
import os
import re
import subprocess
import sys
import time

NS = "argocd"
POLL = float(os.environ.get("POLL", "0.25"))
TIMEOUT = float(os.environ.get("TIMEOUT", "900"))


# The driver's own API requests, so a report can tell them from Argo CD's:
# kubectl invocations, and how many of them were single `get --raw` GETs.
COUNTER = os.environ.get("HARNESS_COUNTER", "/tmp/argocd-harness.json")


def count(raw):
    try:
        with open(COUNTER) as f:
            c = json.load(f)
    except (OSError, ValueError):
        c = {"kubectl": 0, "raw_gets": 0}
    c["kubectl"] += 1
    c["raw_gets"] += int(raw)
    with open(COUNTER, "w") as f:
        json.dump(c, f)


def kubectl(*args, check=True):
    count(False)
    p = subprocess.run(["kubectl", *args], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"kubectl {' '.join(args)}: {p.stderr.strip()}")
    return p.stdout


PATHS = {
    "pods": "/api/v1/namespaces/{ns}/pods",
    "deployments": "/apis/apps/v1/namespaces/{ns}/deployments",
    "statefulsets": "/apis/apps/v1/namespaces/{ns}/statefulsets",
    "applications": "/apis/argoproj.io/v1alpha1/namespaces/{ns}/applications",
}


def get_json(resource, ns, name=None):
    """One GET straight to the resource's path (no discovery): None if absent."""
    path = PATHS[resource].format(ns=ns) + (f"/{name}" if name else "")
    count(True)
    p = subprocess.run(["kubectl", "get", "--raw", path], capture_output=True, text=True)
    try:
        return json.loads(p.stdout) if p.returncode == 0 else None
    except ValueError:
        return None


def wait(pred, what, timeout=TIMEOUT):
    t0 = time.monotonic()
    last = None
    while time.monotonic() - t0 < timeout:
        last = pred()
        if last is True:
            return
        time.sleep(POLL)
    raise SystemExit(f"timed out after {timeout:.0f}s waiting for {what}: {last}")


def pod_ready(p):
    return any(c.get("type") == "Ready" and c.get("status") == "True" for c in p.get("status", {}).get("conditions", []))


def argocd_pods():
    pods = (get_json("pods", NS) or {}).get("items", [])
    # Hook Jobs (redis-secret-init) end Succeeded; they do not count.
    return [p for p in pods if p.get("status", {}).get("phase") != "Succeeded"]


def install(chart, values):
    t0 = time.monotonic()
    subprocess.run(["helm", "install", "argocd", chart, "-n", NS, "--create-namespace", "-f", values],
                   check=True, stdout=subprocess.DEVNULL)
    helm_s = time.monotonic() - t0
    ready_at = {}

    def all_ready():
        pods = argocd_pods()
        now = time.monotonic() - t0
        for p in pods:
            if pod_ready(p):
                ready_at.setdefault(p["metadata"]["name"], round(now, 2))
        # Every Deployment/StatefulSet has created its pods and all are Ready.
        workloads = [w for kind in ("deployments", "statefulsets")
                     for w in (get_json(kind, NS) or {}).get("items", [])]
        want = sum(w.get("spec", {}).get("replicas", 1) for w in workloads)
        ready = sum(1 for p in pods if pod_ready(p) and not p["metadata"].get("deletionTimestamp"))
        return True if workloads and ready >= want and len(pods) == want else f"{ready}/{want} ready"

    wait(all_ready, "Argo CD pods Ready")
    total = time.monotonic() - t0
    pods = argocd_pods()
    return {
        "helm_install_s": round(helm_s, 2),
        "all_ready_s": round(total, 2),
        "pods": {p["metadata"]["name"]: {
            "ready_s": ready_at.get(p["metadata"]["name"]),
            "containers": [c["image"] for c in p["spec"]["containers"]],
            "restarts": sum(c.get("restartCount", 0) for c in p.get("status", {}).get("containerStatuses", [])),
        } for p in pods},
    }


def uninstall():
    subprocess.run(["helm", "uninstall", "argocd", "-n", NS, "--wait"], check=True, stdout=subprocess.DEVNULL)
    wait(lambda: True if not (get_json("pods", NS) or {}).get("items") else "pods left", "Argo CD pods gone")
    return {}


def app(name):
    return get_json("applications", NS, name)


def app_state(a):
    st = (a or {}).get("status", {})
    return st.get("sync", {}).get("status"), st.get("health", {}).get("status")


def synced_healthy(name, extra=lambda: True):
    def check():
        a = app(name)
        sync, health = app_state(a)
        op = (a or {}).get("status", {}).get("operationState", {}).get("phase")
        if sync == "Synced" and health == "Healthy" and op in (None, "Succeeded"):
            e = extra()
            return True if e is True else e
        return f"sync={sync} health={health} op={op}"
    return check


def app_create(manifest, name):
    t0 = time.monotonic()
    kubectl("apply", "-f", manifest)
    wait(synced_healthy(name), f"{name} Synced+Healthy")
    return {"seconds": round(time.monotonic() - t0, 2)}


def app_delete(name):
    a = app(name)
    if not a:
        return {}
    dest = a["spec"]["destination"].get("namespace", "default")
    kubectl("delete", "applications.argoproj.io", "-n", NS, name, "--wait=true", "--timeout=300s")
    # Each app has its own destination namespace: wait for it to empty.
    wait(lambda: True if not ((get_json("deployments", dest) or {}).get("items") or (get_json("pods", dest) or {}).get("items")) else "resources left",
         f"{name} resources gone")
    return {}


def deployment_ready(ns, name, replicas=None):
    def check():
        d = get_json("deployments", ns, name)
        if not d:
            return "absent"
        want = d["spec"].get("replicas", 1) if replicas is None else replicas
        st = d.get("status", {})
        ok = (d["spec"].get("replicas", 1) == want and st.get("readyReplicas", 0) == want
              and st.get("updatedReplicas", 0) == want and st.get("replicas", 0) == want
              and st.get("observedGeneration", 0) >= d["metadata"].get("generation", 0))
        return True if ok else f"ready={st.get('readyReplicas', 0)}/{want}"
    return check


def app_scale(name, replicas):
    a = app(name)
    dest = a["spec"]["destination"].get("namespace", "default")
    patch = {"spec": {"source": {"helm": {"parameters": [{"name": "replicaCount", "value": str(replicas)}]}}}}
    t0 = time.monotonic()
    kubectl("patch", "applications.argoproj.io", "-n", NS, name, "--type", "merge", "-p", json.dumps(patch))
    deploy = name  # helm-guestbook names its Deployment after the release
    wait(synced_healthy(name, deployment_ready(dest, deploy, int(replicas))), f"{name} at {replicas} replicas")
    return {"seconds": round(time.monotonic() - t0, 2), "replicas": int(replicas)}


def drift(name, ns, deployment):
    old = get_json("deployments", ns, deployment)["metadata"]["uid"]
    t0 = time.monotonic()
    kubectl("delete", "deployment", "-n", ns, deployment, "--wait=false")
    seen = {}

    def back():
        d = get_json("deployments", ns, deployment)
        if d and d["metadata"]["uid"] != old:
            seen.setdefault("recreated_s", round(time.monotonic() - t0, 2))
            r = deployment_ready(ns, deployment)()
            if r is True:
                return synced_healthy(name)()
            return r
        return "not recreated"

    wait(back, f"{deployment} recreated by {name}")
    return {"seconds": round(time.monotonic() - t0, 2), **seen}


LINE = re.compile(r'^(apiserver_request_total|apiserver_longrunning_requests)\{(.*)\} ([0-9.e+]+)$')


def metrics():
    count(True)
    raw = subprocess.run(["kubectl", "get", "--raw", "/metrics"], capture_output=True, text=True, check=True).stdout
    out = {"apiserver_request_total": [], "apiserver_longrunning_requests": []}
    for line in raw.splitlines():
        m = LINE.match(line)
        if not m:
            continue
        labels = dict(re.findall(r'(\w+)="([^"]*)"', m.group(2)))
        keep = {k: labels.get(k, "") for k in ("verb", "group", "version", "resource", "subresource", "scope", "code")}
        if m.group(1) == "apiserver_longrunning_requests":
            keep.pop("code")
        keep["value"] = float(m.group(3))
        out[m.group(1)].append(keep)
    out["t"] = time.time()
    try:
        with open(COUNTER) as f:
            out["harness"] = json.load(f)
    except (OSError, ValueError):
        out["harness"] = {"kubectl": 0, "raw_gets": 0}
    return out


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    fn = {"install": install, "uninstall": uninstall, "app-create": app_create, "app-delete": app_delete,
          "app-scale": app_scale, "drift": drift, "metrics": metrics}.get(cmd)
    if fn is None:
        raise SystemExit(__doc__)
    json.dump(fn(*args), sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
