#!/usr/bin/env python3
"""Render the argocd phases of a bench/run.sh results directory as Markdown.

  python3 bench/argocd_report.py bench/results/<timestamp> [more dirs...]

With several directories (repeat runs), times are pooled and memory/CPU
numbers are shown per run.
"""
import json
import os
import statistics
import sys

SYSTEMS = [("argocd-k3s", "k3s minimal"), ("argocd-litek8s", "litek8s (crun)")]


def load(d, name):
    path = os.path.join(d, name)
    if not os.path.exists(path):
        return None
    with open(path) as f:
        if name.endswith(".jsonl"):
            return [json.loads(l) for l in f if l.strip()]
        text = f.read().strip()
        return json.loads(text) if text else None


def mib(kib):
    return f"{kib / 1024:.0f}"


def med(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "–"
    m = statistics.median(xs)
    return f"{m:.1f} s" if len(xs) == 1 else f"{m:.1f} s ({min(xs):.1f}–{max(xs):.1f}, n={len(xs)})"


def cat(mem, key):
    return mem["categories"].get(key, {}).get("pss_kib", 0)


ORCH = ("control-plane", "runtime", "addons")


def orchestrator(mem):
    return sum(cat(mem, k) for k in ORCH)


# Pod -> Argo CD component, from the processes it runs.
COMPONENTS = [("argocd-applicat", "application/applicationset controller"), ("argocd-server", "server"),
              ("argocd-repo-ser", "repo server"), ("argocd-notifica", "notifications controller"),
              ("argocd-dex", "dex"), ("dex", "dex"), ("redis-server", "redis")]


def components(mem):
    out = {}
    for pod in mem.get("pods", {}).values():
        if pod["kind"] != "argocd":
            continue
        name = next((label for c, label in COMPONENTS if any(x.startswith(c) for x in pod["comms"])), "other")
        out[name] = out.get(name, 0) + pod["pss_kib"]
    return out


def harness(before, after):
    """The benchmark driver's own requests in the window: (raw GETs, other kubectl runs)."""
    b, a = before.get("harness", {}), after.get("harness", {})
    raw = a.get("raw_gets", 0) - b.get("raw_gets", 0)
    return raw, a.get("kubectl", 0) - b.get("kubectl", 0) - raw


def api_delta(before, after):
    """Requests in the window by (verb, resource), and the duration."""
    key = lambda r: tuple(sorted((k, v) for k, v in r.items() if k != "value"))
    base = {key(r): r["value"] for r in before["apiserver_request_total"]}
    out = {}
    for r in after["apiserver_request_total"]:
        n = r["value"] - base.get(key(r), 0)
        if n > 0:
            res = r["resource"] + (f"/{r['subresource']}" if r["subresource"] else "") or "(discovery, openapi, ...)"
            k = (r["verb"], res)
            out[k] = out.get(k, 0) + n
    return out, after["t"] - before["t"]


def watches(m):
    return int(sum(r["value"] for r in m["apiserver_longrunning_requests"] if r["verb"] == "WATCH"))


def main():
    dirs = sys.argv[1:]
    p = print
    present = [(t, l) for t, l in SYSTEMS if any(load(d, f"{t}.install-cold.json") for d in dirs)]
    if not present:
        sys.exit("no argocd phase results")
    env = load(dirs[0], "env.json") or {}
    p(f"Argo CD chart {env.get('argo_chart_version')} (defaults), {env.get('argo_vm_cpus')} vCPU / "
      f"{env.get('argo_vm_mem_mib')} MiB {env.get('guest_arch', 'arm64')} guests on {env.get('host')}, litek8s `{env.get('litek8s_commit')}`; "
      f"runs: {', '.join(os.path.basename(d.rstrip('/')) for d in dirs)}\n")

    # ---------------------------------------------------------------- times
    p("### Time\n")
    p("| | " + " | ".join(l for _, l in present) + " |")
    p("|---|" + "---|" * len(present))
    rows = {}
    for t, _ in present:
        r = rows.setdefault(t, {})
        for d in dirs:
            cold = load(d, f"{t}.install-cold.json")
            warm = load(d, f"{t}.install-warm.json") or []
            if cold:
                r.setdefault("cold", []).append(cold["all_ready_s"])
            r.setdefault("warm", []).extend(w["all_ready_s"] for w in warm)
            r.setdefault("helm", []).extend(w["helm_install_s"] for w in warm)
            for app in ("guestbook", "helm-guestbook"):
                runs = load(d, f"{t}.app-create.{app}.json") or []
                if runs:
                    r.setdefault(f"{app}-cold", []).append(runs[0]["seconds"])
                    r.setdefault(f"{app}-warm", []).extend(x["seconds"] for x in runs[1:])
            for s in load(d, f"{t}.app-scale.json") or []:
                r.setdefault(f"scale-{s['replicas']}", []).append(s["seconds"])
            for x in load(d, f"{t}.drift.json") or []:
                r.setdefault("drift-recreated", []).append(x.get("recreated_s"))
                r.setdefault("drift", []).append(x["seconds"])
    labels = [
        ("cold", "`helm install` → all Argo CD pods Ready, images pulled"),
        ("warm", "… again after `helm uninstall`, images cached"),
        ("helm", "… of which `helm install` itself (pre-install hook Job)"),
        ("guestbook-cold", "Application `guestbook` → Synced + Healthy, first (clone, image pull)"),
        ("guestbook-warm", "… recreated (cached)"),
        ("helm-guestbook-cold", "Application `helm-guestbook` → Synced + Healthy, first"),
        ("helm-guestbook-warm", "… recreated"),
        ("scale-2", "helm parameter replicaCount 1 → 2: re-synced, 2 pods Ready"),
        ("scale-1", "replicaCount 2 → 1: re-synced"),
        ("drift-recreated", "managed Deployment deleted → recreated by self-heal"),
        ("drift", "… → Available again, app Synced + Healthy"),
    ]
    for k, label in labels:
        p(f"| {label} | " + " | ".join(med(rows[t].get(k, [])) for t, _ in present) + " |")

    # ---------------------------------------------------------------- memory
    for d in dirs:
        p(f"\n### Memory, PSS MiB (`{os.path.basename(d.rstrip('/'))}`)\n")
        p("| | " + " | ".join(l for _, l in present) + " |")
        p("|---|" + "---|" * len(present))
        mems = {t: {s: load(d, f"{t}.{s}.mem.json") for s in ("idle0", "argo-idle", "idle-app", "idle-app-end")} for t, _ in present}
        samples = {t: load(d, f"{t}.mem-sync.jsonl") or [] for t, _ in present}

        def row(label, f):
            vals = []
            for t, _ in present:
                try:
                    vals.append(f(t))
                except (TypeError, KeyError, ValueError):
                    vals.append("–")
            p(f"| {label} | " + " | ".join(vals) + " |")

        row("Orchestrator, no Argo CD (control plane + runtime + addons)", lambda t: mib(orchestrator(mems[t]["idle0"])))
        row("Orchestrator, Argo CD + 1 app idle", lambda t: mib(orchestrator(mems[t]["idle-app"])))
        row("… control plane (k3s-server / litek8s apiserver)", lambda t: mib(cat(mems[t]["idle-app"], "control-plane")))
        row("… container runtime (containerd, shims, pause / crun shims)", lambda t: mib(cat(mems[t]["idle-app"], "runtime")))
        row("… addon pods (CoreDNS)", lambda t: mib(cat(mems[t]["idle-app"], "addons")))
        row("Argo CD pods, idle with 1 app", lambda t: mib(cat(mems[t]["idle-app"], "argocd")))
        row("Orchestrator, peak while syncing (5 s samples)", lambda t: mib(max(orchestrator(s) for s in samples[t])))
        row("Argo CD pods, peak while syncing", lambda t: mib(max(cat(s, "argocd") for s in samples[t])))
        row("Guest MemAvailable, no Argo CD", lambda t: mib(mems[t]["idle0"]["system"]["mem_available_kib"]))
        row("Guest MemAvailable, Argo CD + 1 app idle", lambda t: mib(mems[t]["idle-app"]["system"]["mem_available_kib"]))
        row("Guest MemAvailable, lowest while syncing", lambda t: mib(min(s["system"]["mem_available_kib"] for s in samples[t])))
        row("Control plane after the 5 min idle window", lambda t: mib(cat(mems[t]["idle-app-end"], "control-plane")))
        comps = sorted({c for t, _ in present for c in components(mems[t]["idle-app"] or {})})
        for c in comps:
            row(f"Argo CD {c}", lambda t, c=c: mib(components(mems[t]["idle-app"]).get(c, 0)))

    # ---------------------------------------------------------------- cpu
    for d in dirs:
        p(f"\n### Idle CPU with Argo CD and 1 synced app, % of one vCPU (`{os.path.basename(d.rstrip('/'))}`)\n")
        cpus = {t: load(d, f"{t}.idle-app.cpu.json") for t, _ in present}
        p("| | " + " | ".join(l for _, l in present) + " |")
        p("|---|" + "---|" * len(present))
        if all(cpus.values()):
            p("| whole guest busy | " + " | ".join(f"{100 * c['busy_cpu_s'] / c['window_s']:.2f}" for c in cpus.values()) + " |")
            for k in ("control-plane", "runtime", "addons", "argocd", "workload", "os/other"):
                p(f"| {k} | " + " | ".join(f"{c['categories'].get(k, {}).get('pct_one_core', 0):.2f}" for c in cpus.values()) + " |")

    # ---------------------------------------------------------------- host
    def busy(a, b):
        if not a or not b or "busy_ticks" not in a:
            return None
        return 100.0 * (b["busy_ticks"] - a["busy_ticks"]) / max(1, b["total_ticks"] - a["total_ticks"])

    rows = []
    for d in dirs:
        for t, label in present:
            snaps = {k: load(d, f"{t}.host.{k}.json") for k in ("start", "idle-start", "idle-end", "end")}
            if not snaps["start"] or "loadavg" not in snaps["start"]:
                continue
            phase, idle = busy(snaps["start"], snaps["end"]), busy(snaps["idle-start"], snaps["idle-end"])
            rows.append(f"| {os.path.basename(d.rstrip('/'))} | {label} | {phase:.1f} % | {idle:.1f} % | "
                        f"{snaps['start']['loadavg'][0]} → {snaps['end']['loadavg'][0]} |")
    if rows:
        p("\n### Host load while each phase ran (all host CPUs, the VM included)\n")
        p("| run | system | host busy, whole phase | host busy, idle window | load avg (1 min) start → end |")
        p("|---|---|---|---|---|")
        for r in rows:
            p(r)

    # ---------------------------------------------------------------- api
    for d in dirs:
        p(f"\n### API requests seen by the API server (`{os.path.basename(d.rstrip('/'))}`)\n")
        windows = []
        for t, _ in present:
            i0 = load(d, f"{t}.api-idle0.json")
            w = {"idle0": api_delta(i0["before"], i0["after"]) if i0 else None}
            for name in ("sync", "idle"):
                b, a = load(d, f"{t}.api-{name}.before.json"), load(d, f"{t}.api-{name}.after.json")
                w[name] = api_delta(b, a) if b and a else None
                w[f"{name}-harness"] = harness(b, a) if b and a else None
            w["watches"] = watches(load(d, f"{t}.api-idle.after.json")) if load(d, f"{t}.api-idle.after.json") else None
            w["watches0"] = watches(i0["after"]) if i0 else None
            windows.append(w)
        rate = lambda x: f"{sum(x[0].values()) / x[1]:.2f}/s" if x else "–"
        p("| | " + " | ".join(l for _, l in present) + " |")
        p("|---|" + "---|" * len(present))
        p("| no Argo CD (60 s) | " + " | ".join(rate(w["idle0"]) for w in windows) + " |")
        p("| Argo CD + 1 app idle | " + " | ".join(rate(w["idle"]) for w in windows) + " |")
        p("| during the GitOps steps (all clients) | " + " | ".join(rate(w["sync"]) for w in windows) + " |")
        p("| … of which the benchmark driver: GETs + other kubectl runs | " + " | ".join(
            f"{w['sync-harness'][0]} + {w['sync-harness'][1]}" if w.get("sync-harness") else "–" for w in windows) + " |")
        p("| during the GitOps steps without the driver's GETs | " + " | ".join(
            f"{(sum(w['sync'][0].values()) - w['sync-harness'][0]) / w['sync'][1]:.2f}/s" if w.get("sync") and w.get("sync-harness") else "–"
            for w in windows) + " |")
        p("| open watches, no Argo CD | " + " | ".join(str(w["watches0"]) for w in windows) + " |")
        p("| open watches, Argo CD + 1 app | " + " | ".join(str(w["watches"]) for w in windows) + " |")
        for (t, label), w in zip(present, windows):
            for name in ("idle", "sync"):
                if not w[name]:
                    continue
                reqs, secs = w[name]
                top = sorted(reqs.items(), key=lambda kv: -kv[1])[:12]
                p(f"\n{label}, {name} window ({secs:.0f} s, {int(sum(reqs.values()))} requests): " +
                  ", ".join(f"{v} {r} {int(n)}" for (v, r), n in top))


if __name__ == "__main__":
    main()
