#!/usr/bin/env python3
"""Summarize conformance JUnit results: compat/conformance_summary.py <results-dir> [target].

Reads every junit*.xml below the directory, keeps the [Conformance] specs that
ran (ginkgo reports the filtered-out ones as skipped), and prints markdown:
the total, a per-SIG table and the failing spec names.
"""
import collections
import glob
import os
import re
import sys
import xml.etree.ElementTree as ET


def load(root):
    cases = {}
    for path in sorted(glob.glob(os.path.join(root, "**", "junit*.xml"), recursive=True)):
        for tc in ET.parse(path).getroot().iter("testcase"):
            name = tc.get("name", "")
            if "[Conformance]" not in name:
                continue
            failure = tc.find("failure")
            if failure is None:
                failure = tc.find("error")
            if failure is not None:
                status = "failed"
            elif tc.find("skipped") is not None:
                status = "skipped"
            else:
                status = "passed"
            # A spec runs in exactly one of the parallel/serial passes; prefer a real result.
            if name in cases and status == "skipped":
                continue
            msg = ""
            if failure is not None:
                msg = (failure.get("message") or failure.text or "").strip().splitlines()
                msg = msg[0][:200] if msg else ""
            cases[name] = (status, msg, float(tc.get("time") or 0))
    return cases


def sig(name):
    m = re.search(r"\[sig-([a-z-]+)\]", name)
    return m.group(1) if m else "other"


def trim(root):
    """Keep only the [Conformance] test cases in each JUnit file (the e2e
    binary reports every spec it skipped too), without their timelines,
    and compress the logs."""
    import gzip
    import shutil
    for path in glob.glob(os.path.join(root, "**", "junit*.xml"), recursive=True):
        tree = ET.parse(path)
        for suite in tree.getroot().iter("testsuite"):
            for tc in list(suite.findall("testcase")):
                if "[Conformance]" not in tc.get("name", ""):
                    suite.remove(tc)
                    continue
                # Each spec's timeline is in the gzipped log already.
                for out in tc.findall("system-err") + tc.findall("system-out"):
                    tc.remove(out)
        tree.write(path, encoding="utf-8", xml_declaration=True)
    for path in glob.glob(os.path.join(root, "**", "e2e.log"), recursive=True):
        with open(path, "rb") as src, gzip.open(path + ".gz", "wb", compresslevel=9) as dst:
            shutil.copyfileobj(src, dst)
        os.remove(path)


def main():
    if sys.argv[1] == "--trim":
        trim(sys.argv[2])
        return
    root = sys.argv[1]
    target = sys.argv[2] if len(sys.argv) > 2 else os.path.basename(root.rstrip("/"))
    cases = load(root)
    ran = {n: v for n, v in cases.items() if v[0] != "skipped"}
    passed = sum(1 for v in ran.values() if v[0] == "passed")
    seconds = ""
    try:
        seconds = open(os.path.join(root, "seconds")).read().strip()
    except OSError:
        pass
    print(f"# Conformance: {target}\n")
    print(f"**{passed}/{len(ran)} passed** ({len(cases) - len(ran)} skipped)"
          + (f", {int(seconds) // 60} min wall clock" if seconds else "") + "\n")
    by_sig = collections.defaultdict(lambda: [0, 0, 0])
    for n, (status, _, _) in cases.items():
        row = by_sig[sig(n)]
        if status == "passed":
            row[0] += 1
        if status != "skipped":
            row[1] += 1
        else:
            row[2] += 1
    print("| SIG | passed | ran | skipped |\n|---|---|---|---|")
    for s in sorted(by_sig):
        p, r, k = by_sig[s]
        print(f"| {s} | {p} | {r} | {k} |")
    failed = sorted(n for n, v in ran.items() if v[0] == "failed")
    if failed:
        print(f"\n## Failing ({len(failed)})\n")
        for n in failed:
            msg = ran[n][1].replace("|", "\\|")
            print(f"- {n}" + (f"  \n  `{msg}`" if msg else ""))
    skipped = sorted(n for n, v in cases.items() if v[0] == "skipped")
    if skipped and len(skipped) < 50:
        print(f"\n## Skipped ({len(skipped)})\n")
        for n in skipped:
            print(f"- {n}")


if __name__ == "__main__":
    main()
