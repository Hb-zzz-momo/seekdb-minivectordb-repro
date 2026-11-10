#!/usr/bin/env python3
"""Statements that touch more data than the memory limit, inside a real cgroup.

Loads ROWS x 128-d rows (about 560 bytes each) into a database running in a
transient systemd user unit with MemoryMax=MEM and no swap, then runs a
full-table UPDATE, a restart, a half-table DELETE, an UPDATE inside a
transaction and a kill -9 + restart, checking every answer. Exit status 0
only if every step answered correctly and the unit was never OOM-killed.

  cargo build --release
  python3 tests/mem_probe.py [--rows 100000] [--mem 64M] [--bin PATH] [--env K=V ...]

The default is 51 MiB of rows under a 64 MiB limit: the UPDATE rewrites all
of it in one statement, as one log frame, which the restart then replays.
"""
import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile

ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--bin", default="target/release/minivectordb")
ap.add_argument("--rows", type=int, default=100000)
ap.add_argument("--mem", default="64M")
ap.add_argument("--env", action="append", default=[])
a = ap.parse_args()
work = tempfile.mkdtemp(prefix="mem-probe-")
data, rnd, n_units = os.path.join(work, "data"), random.Random(7), [0]
os.makedirs(data)


def fail(msg):
    print(f"FAIL: {msg}\n  data kept in {work}")
    sys.exit(1)


class Db:
    """The database in its own cgroup, behind the judge's protocol."""

    def __init__(self):
        n_units[0] += 1
        self.unit, self.seq = f"mvdb-mem-{os.getpid()}-{n_units[0]}", 0
        cmd = ["systemd-run", "--user", "--pipe", "--quiet", "--collect", f"--unit={self.unit}",
               "-p", "MemoryAccounting=yes", "-p", f"MemoryMax={a.mem}", "-p", "MemorySwapMax=0"]
        cmd += [f"--setenv={e}" for e in a.env]
        self.p = subprocess.Popen(cmd + [os.path.abspath(a.bin), "--data-dir", data],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        if self.p.stdout.readline().strip() != b'{"type":"ready"}':
            fail("no ready line (killed during recovery?)")

    def request(self, what, **fields):
        self.seq += 1
        try:
            self.p.stdin.write((json.dumps({"id": self.seq, **fields}, ensure_ascii=False) + "\n").encode())
            self.p.stdin.flush()
            line = self.p.stdout.readline()
        except BrokenPipeError:
            line = b""
        if not line:
            fail(f"no answer (process died, exit {self.p.wait()}) to: {what[:70]}")
        r = json.loads(line)
        if r.get("id") != self.seq:
            fail(f"answer {line[:200]!r} to request {self.seq}")
        return r

    def run(self, sql, **want):
        """Executes `sql`; with `want`, the answer must hold exactly those fields."""
        r = self.request(sql, op="execute", sql=sql, search_mode="exact")
        if r.get("ok") is not True or any(r.get(k) != v for k, v in want.items()):
            fail(f"{sql[:70]}\n  want {want}\n  got  {json.dumps(r, ensure_ascii=False)[:200]}")
        print(f"  ok  {sql[:58]:58s} {json.dumps({k: r[k] for k in r if k not in ('id', 'ok')})[:60]}")

    def kill(self):
        subprocess.run(["systemctl", "--user", "kill", "-s", "KILL", self.unit + ".service"], check=False)
        self.p.wait()

    def close(self):
        if self.request("shutdown", op="shutdown") != {"id": self.seq, "ok": True}:
            fail("shutdown was not acknowledged")
        self.p.stdin.close()
        if self.p.wait() != 0:
            fail(f"exit status {self.p.returncode} after shutdown")


def vec():
    return "[" + ",".join("%.6f" % rnd.uniform(-1, 1) for _ in range(128)) + "]"


n, half = a.rows, a.rows // 2
db = Db()
db.run("CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR(16), content VARCHAR(32), embedding VECTOR(128));")
for b in range(0, n, 100):
    rows = ",".join(f"({i},'c{i % 10}','row{i}','{vec()}')" for i in range(b, min(n, b + 100)))
    r = db.request("load", op="execute", sql=f"INSERT INTO docs VALUES {rows};", search_mode="exact")
    if r.get("ok") is not True:
        fail(f"load failed at row {b}: {r}")
print(f"  loaded {n} rows under MemoryMax={a.mem}")
db.run("UPDATE docs SET content = 'all';", affected_rows=n)
db.run(f"SELECT id, content FROM docs WHERE id = {n - 1};", rows=[[n - 1, "all"]])
db.close()
db = Db()
db.run("SELECT id, content FROM docs WHERE id = 0;", rows=[[0, "all"]])
db.run(f"DELETE FROM docs WHERE id >= {half};", affected_rows=n - half)
db.run("BEGIN;")
db.run("UPDATE docs SET content = 'open' WHERE id < 100;", affected_rows=100)
db.run("SELECT id, content FROM docs WHERE id = 5;", rows=[[5, "open"]])
db.kill()
db = Db()
db.run(f"SELECT id, content FROM docs WHERE id >= {half - 1};", rows=[[half - 1, "all"]])
db.run("SELECT id, content FROM docs WHERE id = 5;", rows=[[5, "all"]])
db.run("UPDATE docs SET category = 'z' WHERE id >= 0;", affected_rows=half)
db.run(f"SELECT id, category FROM docs WHERE id = {half - 1};", rows=[[half - 1, "z"]])
db.close()
shutil.rmtree(work, ignore_errors=True)
print("PASS")
