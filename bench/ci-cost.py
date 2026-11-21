#!/usr/bin/env python3
"""What ci-bench.yml's jobs cost on GitHub-hosted runners, per run and per month.

  gh run download <run id> -n results          # results.json of a ci-bench run
  python3 bench/ci-cost.py results.json [--runs-per-month 10000] [--rate 0.006]
  python3 bench/ci-cost.py results.json --markdown   # what the report job writes

GitHub bills every job by its duration rounded up to the whole minute, so a
job of 61 s costs two minutes: the billed minutes are the mean of each run's
rounded-up duration, not the median duration divided by 60. Only failed
runs are left out, and the table says how many runs remain.

The default rate is GitHub's standard Linux x64 2-core runner, $0.006 per
minute since the price cut of 2026-01-01 (check GitHub's pricing page before
quoting it). Minutes of public repositories are free; private ones pay
beyond the plan's included minutes (--included-minutes, subtracted from
each engine's monthly minutes).
"""
import argparse
import json
import math
import statistics
from collections import defaultdict

BASE = "litek8s"


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def median(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def summarize(records):
    groups = defaultdict(list)
    for r in records:
        groups[(r["example"], r["engine"])].append(r)
    rows = []
    for (example, engine), rs in sorted(groups.items()):
        ok = [r for r in rs if r.get("ok", True)]
        billed = [r["billed_minutes"] for r in ok if r.get("billed_minutes") is not None]
        if not billed:
            # The in-container jobs and older runs have no billed minutes:
            # derive them from the job total when there is one.
            billed = [math.ceil(num(r["job_seconds"]) / 60) for r in ok if num(r.get("job_seconds"))]
        rows.append({
            "example": example,
            "engine": engine,
            "runs": len(ok),
            "failed": len(rs) - len(ok),
            "ready": median(num(r.get("ready_seconds")) for r in ok),
            "steps": median(num(r.get("steps_seconds")) for r in ok),
            "job": median(num(r.get("job_seconds")) for r in ok),
            "billed": statistics.mean(billed) if billed else None,
            "peak": median(num(r.get("peak_mib")) for r in ok),
        })
    return rows


def fmt(v, digits=1, unit=""):
    return "" if v is None else f"{v:,.{digits}f}{unit}"


def money(v):
    return "" if v is None else f"${v:,.2f}"


def table(rows, args):
    for r in rows:
        r["per_run"] = None if r["billed"] is None else r["billed"] * args.rate
        minutes = None if r["billed"] is None else max(r["billed"] * args.runs_per_month - args.included_minutes, 0)
        r["month"] = None if minutes is None else minutes * args.rate
        r["hours"] = None if r["job"] is None else r["job"] * args.runs_per_month / 3600
    base = {r["example"]: r for r in rows if r["engine"] == BASE}
    out = []
    head = ["example", "engine", "runs", "ready (s)", "steps (s)", "job (s)", "billed min/run",
            "$ / run", f"$ / month ({args.runs_per_month:,} runs)", "runner hours / month", f"vs {BASE}"]
    out.append(head)
    for r in rows:
        b = base.get(r["example"])
        vs = ""
        if b and r is not b and b["billed"] and r["billed"]:
            saved = r["month"] - b["month"]
            vs = f"{r['billed'] / b['billed']:.1f}x the minutes, {money(saved)} / month more"
        runs = f"{r['runs']}" + (f" ({r['failed']} failed)" if r["failed"] else "")
        out.append([r["example"], r["engine"], runs, fmt(r["ready"]), fmt(r["steps"]), fmt(r["job"], 0),
                    fmt(r["billed"], 2), f"${r['per_run']:.4f}" if r["per_run"] is not None else "",
                    money(r["month"]), fmt(r["hours"], 0), vs])
    return out


def render(out, markdown):
    if markdown:
        lines = ["| " + " | ".join(out[0]) + " |", "|" + "---|" * len(out[0])]
        lines += ["| " + " | ".join(row) + " |" for row in out[1:]]
        return "\n".join(lines)
    widths = [max(len(row[i]) for row in out) for i in range(len(out[0]))]
    return "\n".join("  ".join(c.ljust(w) for c, w in zip(row, widths)).rstrip() for row in out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("results", help="results.json of a ci-bench run")
    ap.add_argument("--rate", type=float, default=0.006, help="$ per billed minute (default: Linux x64 2-core)")
    ap.add_argument("--runs-per-month", type=int, default=10000, help="runs of each job per month")
    ap.add_argument("--included-minutes", type=int, default=0, help="minutes the plan includes per month")
    ap.add_argument("--markdown", action="store_true")
    args = ap.parse_args()
    with open(args.results) as f:
        rows = summarize(json.load(f))
    print(render(table(rows, args), args.markdown))
    print()
    print(f"${args.rate}/min, each job's duration rounded up to the minute; "
          f"medians of the successful runs; {args.runs_per_month:,} runs per month"
          + (f", {args.included_minutes:,} included minutes" if args.included_minutes else "") + ".")


if __name__ == "__main__":
    main()
