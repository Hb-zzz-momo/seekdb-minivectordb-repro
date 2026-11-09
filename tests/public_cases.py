#!/usr/bin/env python3
"""Runs the organizers' public SQL cases (mysqltest/) against minivectordb.

Each case in mysqltest/manifest.json gets a fresh, empty data directory. The
non-empty lines of its `test` file are SQL statements or driver directives;
the SQL goes to the database one statement per line (JSON-lines output) and
each result is compared with the assertion at the same position in its
`result` file, following mysqltest/README.md:

- `{}`: the statement succeeds;
- `error`: the statement fails with that semantic error name;
- `affected_rows`: exact;
- `columns`: names and order exact;
- `rows`: in order, or as a multiset when `unordered` is true. Floats match
  within 1e-5 absolute or relative; integers and strings exactly.

Directives take no assertion position:

- `--restart`: close stdin (a normal shutdown, which must roll back an open
  transaction), wait for the exit and start again on the same directory;
- `--mode ann`: later vector queries may be approximate. How the official
  driver tells the database is not published, so the runner only notes it;
  the exact answers this database gives are valid approximate ones.

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
    if "error" in want:
        if "error" not in got:
            return f"succeeded, expected error {want['error']}"
        if got["error"] != want["error"]:
            return f"error {got['error']}, expected {want['error']}"
        return None
    if "error" in got or not got.get("ok", "columns" in got):
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


DIRECTIVES = {"--restart", "--mode ann", "--mode exact"}


def run_session(binary, data_dir, sql):
    """Runs one database process over `sql`; returns (results, problem)."""
    p = subprocess.run([binary, "--data-dir", data_dir], input="".join(s + "\n" for s in sql), capture_output=True,
                       text=True, encoding="utf-8", env={**os.environ, "MVDB_FORMAT": "jsonl"}, timeout=60)
    out = [json.loads(line) for line in p.stdout.splitlines() if line.strip()]
    if not out or out[0].get("ready") is not True:
        return [], f"no ready line (exit {p.returncode}): {p.stderr.strip()[-300:]}"
    out = out[1:]
    if len(out) != len(sql):
        return out, f"{len(out)} results for {len(sql)} statements (exit {p.returncode})"
    if p.returncode != 0:
        return out, f"exit status {p.returncode}"
    return out, None


def run_case(binary, cases, case):
    with open(os.path.join(cases, case["test"]), encoding="utf-8") as f:
        lines = [line.strip() for line in f.read().splitlines() if line.strip()]
    with open(os.path.join(cases, case["result"]), encoding="utf-8") as f:
        expected = json.load(f)
    unknown = [l for l in lines if l.startswith("--") and l not in DIRECTIVES]
    if unknown:
        return [f"unknown directives {unknown}"], []
    sql = [l for l in lines if l not in DIRECTIVES]
    if not len(sql) == len(expected) == case["statements"]:
        return [f"{len(sql)} statements, {len(expected)} results, manifest says {case['statements']}"], []
    # Sessions are separated by --restart.
    sessions, notes = [[]], []
    for l in lines:
        if l == "--restart":
            sessions.append([])
        elif l in DIRECTIVES:
            notes.append(l)
        else:
            sessions[-1].append(l)
    if len(sessions) > 1:
        notes.append(f"{len(sessions) - 1} restart(s)")
    errors, results = [], []
    with tempfile.TemporaryDirectory(prefix="mvdb-case-") as d:
        for n, stmts in enumerate(sessions):
            out, problem = run_session(binary, d, stmts)
            results += out + [None] * (len(stmts) - len(out))
            if problem:
                errors.append(f"session {n + 1}: {problem}")
    for i, (s, want) in enumerate(zip(sql, expected)):
        why = check(want, results[i])
        if why:
            errors.append(f"#{i + 1} `{s}`: {why}")
    return errors, notes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/minivectordb")
    ap.add_argument("--cases", default="mysqltest")
    a = ap.parse_args()
    with open(os.path.join(a.cases, "manifest.json"), encoding="utf-8") as f:
        manifest = json.load(f)
    failed = 0
    for case in manifest["cases"]:
        errors, notes = run_case(a.bin, a.cases, case)
        note = f"  ({', '.join(notes)})" if notes else ""
        print(f"{case['id']:30} {'FAIL' if errors else 'ok'}{note}")
        for e in errors:
            print("    " + e)
        failed += bool(errors)
    total = len(manifest["cases"])
    print(f"{total - failed}/{total} public cases passed ({manifest.get('version')})")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
