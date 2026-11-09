#!/usr/bin/env python3
"""Runs the organizers' public SQL cases (mysqltest/) against minivectordb.

Each case in mysqltest/manifest.json gets a fresh, empty data directory. The
lines of cases/<id>.test are sent one statement per line (JSON-lines output)
and each result is compared with the assertion at the same position in
cases/<id>.result, following mysqltest/README.md:

- `{}`: the statement succeeds;
- `affected_rows`: exact;
- `columns`: names and order exact;
- `rows`: in order, or as a multiset when `unordered` is true. Floats match
  within 1e-5 absolute or relative; integers and strings exactly.

    tests/public_cases.py [--bin target/release/minivectordb] [--cases mysqltest]
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile


def same_value(want, got):
    if isinstance(want, bool) or isinstance(got, bool):
        return want is got
    if isinstance(want, float) or isinstance(got, float):
        if not isinstance(got, (int, float)) or not isinstance(want, (int, float)):
            return False
        return abs(want - got) <= 1e-5 or abs(want - got) <= 1e-5 * abs(want)
    return type(want) is type(got) and want == got


def same_rows(want, got):
    return len(want) == len(got) and all(
        len(a) == len(b) and all(same_value(x, y) for x, y in zip(a, b)) for a, b in zip(want, got))


def check(want, got):
    """None when `got` (one JSON-lines result) satisfies `want`, else why not."""
    if got is None:
        return "no result"
    if not got.get("ok", "columns" in got):
        return f"failed: {got}"
    if "affected_rows" in want and got.get("affected") != want["affected_rows"]:
        return f"affected rows {got.get('affected')}, expected {want['affected_rows']}"
    if "columns" in want and got.get("columns") != want["columns"]:
        return f"columns {got.get('columns')}, expected {want['columns']}"
    if "rows" in want:
        rows, expected = got.get("rows"), want["rows"]
        if rows is None:
            return "no rows"
        if want.get("unordered"):
            key = lambda r: json.dumps(r, ensure_ascii=False, sort_keys=True)
            rows, expected = sorted(rows, key=key), sorted(expected, key=key)
        if not same_rows(expected, rows):
            return f"rows {got.get('rows')}, expected {want['rows']}"
    return None


def run_case(binary, cases, case):
    name = case["id"]
    with open(os.path.join(cases, "cases", name + ".test"), encoding="utf-8") as f:
        sql = [line for line in f.read().splitlines() if line.strip()]
    with open(os.path.join(cases, "cases", name + ".result"), encoding="utf-8") as f:
        expected = json.load(f)
    if not len(sql) == len(expected) == case["statements"]:
        return [f"{len(sql)} statements, {len(expected)} results, manifest says {case['statements']}"]
    with tempfile.TemporaryDirectory(prefix="mvdb-case-") as d:
        p = subprocess.run([binary, "--data-dir", d], input="\n".join(sql) + "\n", capture_output=True,
                           text=True, encoding="utf-8", env={**os.environ, "MVDB_FORMAT": "jsonl"}, timeout=60)
    out = [json.loads(line) for line in p.stdout.splitlines() if line.strip()]
    if not out or out[0].get("ready") is not True:
        return [f"no ready line (exit {p.returncode}): {p.stderr.strip()[-300:]}"]
    out = out[1:]
    errors = []
    for i, (s, want) in enumerate(zip(sql, expected)):
        why = check(want, out[i] if i < len(out) else None)
        if why:
            errors.append(f"#{i + 1} `{s}`: {why}")
    if len(out) > len(sql):
        errors.append(f"{len(out)} results for {len(sql)} statements")
    if p.returncode != 0:
        errors.append(f"exit status {p.returncode}")
    return errors


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/minivectordb")
    ap.add_argument("--cases", default="mysqltest")
    a = ap.parse_args()
    with open(os.path.join(a.cases, "manifest.json"), encoding="utf-8") as f:
        manifest = json.load(f)
    failed = 0
    for case in manifest["cases"]:
        errors = run_case(a.bin, a.cases, case)
        print(f"{case['id']:20} {'FAIL' if errors else 'ok'}")
        for e in errors:
            print("    " + e)
        failed += bool(errors)
    total = len(manifest["cases"])
    print(f"{total - failed}/{total} public cases passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
