"""Summarize a compat run: compat/summary.py <results-dir> [target-label]

Reads results.jsonl and prints a Markdown pass-rate table per tier, overall,
and the list of cases that did not pass.
"""

import json
import pathlib
import sys

out_dir = pathlib.Path(sys.argv[1])
target = sys.argv[2] if len(sys.argv) > 2 else out_dir.name
rows = [json.loads(l) for l in (out_dir / "results.jsonl").read_text().splitlines() if l.strip()]
tiers = sorted({r["tier"] for r in rows})
print(f"# Compatibility: {target}\n")
print("| tier | passed | total | rate |\n|---|---|---|---|")
for t in tiers:
    rs = [r for r in rows if r["tier"] == t]
    p = sum(r["status"] == "pass" for r in rs)
    print(f"| {t} | {p} | {len(rs)} | {100 * p / len(rs):.0f} % |")
p = sum(r["status"] == "pass" for r in rows)
if rows:
    print(f"| **all** | **{p}** | **{len(rows)}** | **{100 * p / len(rows):.0f} %** |")
failed = [r for r in rows if r["status"] != "pass"]
if failed:
    print("\n## Not passing\n")
    for r in failed:
        print(f"- `{r['id']}` ({r['tier']}, {r['status']}): {r['detail']}")
