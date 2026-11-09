#!/usr/bin/env python3
"""Benchmark shaped like the task's section 3.3 workload.

Each phase runs the database in a fresh process inside its own transient
systemd user unit, so its cgroup can be read: memory.peak (page cache
included, as on the platform) and cpu.stat. The user systemd manager on this
machine has no io controller, so disk I/O comes from /proc/<pid>/io
read_bytes + write_bytes (bytes the process read from or dirtied for the
block layer). For a single-process database that tracks io.stat rbytes +
wbytes, except that filesystem journal writes are not included.

  write  empty table, then --rows rows in batches of --batch per commit.
         Timed: the batches.
  query  restart on the written data with its page cache dropped (cold
         start), --warmup untimed queries, then --queries TopK queries, every
         second one with an equality filter. Timed: the queries.

Memory and I/O always cover the whole process, from start to the end of the
phase. Query results are checked: row count, no duplicate ids, filter
respected, and Recall@K against exact distances computed here (tie-aware:
an id counts if its true distance is within the true K-th distance).

  python3 bench/bench.py                      # both phases, defaults below
  python3 bench/bench.py --env MVDB_DROP_CACHE=0 --label no-fadvise
"""
import argparse
import array
import hashlib
import json
import math
import os
import random
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CGROUP_ROOT = "/sys/fs/cgroup"
MIB = 1 << 20


def parse_args():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=os.path.join(ROOT, "target/release/minivectordb"))
    ap.add_argument("--rows", type=int, default=10000)
    ap.add_argument("--dim", type=int, default=128)
    ap.add_argument("--batch", type=int, default=100)
    ap.add_argument("--queries", type=int, default=1000)
    ap.add_argument("--warmup", type=int, default=20)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--categories", type=int, default=10)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--style", choices=["multi", "txn"], default="multi",
                    help="multi: one autocommit INSERT of --batch rows; txn: BEGIN, --batch single-row INSERTs, COMMIT")
    ap.add_argument("--phase", choices=["all", "write", "query"], default="all")
    ap.add_argument("--mem-max", default="1G", help="MemoryMax of the database unit")
    ap.add_argument("--env", action="append", default=[], help="KEY=VALUE passed to the database")
    ap.add_argument("--with-distance", action="store_true", help="also select the distance and check it")
    ap.add_argument("--drop-between-queries", action="store_true",
                    help="drop the data files from the page cache after every query (emulates a streaming scan that discards what it read)")
    ap.add_argument("--workdir", default=os.path.join(ROOT, "target/bench"))
    ap.add_argument("--label", default="")
    return ap.parse_args()


# ---------------------------------------------------------------- workload

def f32(xs):
    """Rounds to float32 the way the database stores the values."""
    return array.array("f", xs).tolist()


def gen_workload(a):
    rng = random.Random(a.seed)
    cats = [f"c{i}" for i in range(a.categories)]

    def vec():
        return [round(rng.uniform(-1.0, 1.0), 6) for _ in range(a.dim)]

    rows = [(i, rng.choice(cats), vec()) for i in range(1, a.rows + 1)]
    queries = [(rng.choice(cats) if q % 2 else None, vec()) for q in range(a.warmup + a.queries)]
    return rows, queries


def vec_sql(v):
    return "'[" + ",".join(f"{x:.6f}" for x in v) + "]'"


def insert_sql(a, batch):
    if a.style == "multi":
        vals = ", ".join(f"({i}, '{c}', {vec_sql(v)})" for i, c, v in batch)
        return [f"INSERT INTO items (id, category, embedding) VALUES {vals};"]
    return (["BEGIN;"]
            + [f"INSERT INTO items (id, category, embedding) VALUES ({i}, '{c}', {vec_sql(v)});" for i, c, v in batch]
            + ["COMMIT;"])


def query_sql(a, cat, v):
    dist = f"L2_DISTANCE(embedding, {vec_sql(v)})"
    cols = f"id, {dist} AS d" if a.with_distance else "id"
    where = f" WHERE category = '{cat}'" if cat is not None else ""
    return f"SELECT {cols} FROM items{where} ORDER BY {dist} LIMIT {a.k};"


def ground_truth(a, rows, queries):
    """Exact distances of every query's true top K, cached per workload."""
    key = hashlib.sha256(json.dumps([a.seed, a.rows, a.dim, a.categories, a.warmup, a.queries, a.k]).encode()).hexdigest()[:16]
    path = os.path.join(a.workdir, f"truth-{key}.json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    t0 = time.perf_counter()
    stored = [(i, c, f32(v)) for i, c, v in rows]
    truth = []
    for cat, qv in queries[a.warmup:]:
        q = f32(qv)
        ds = sorted((math.dist(v, q), i) for i, c, v in stored if cat is None or c == cat)
        top = ds[:a.k]
        truth.append({"ids": [i for _, i in top], "kth": top[-1][0] if top else None, "n": len(ds),
                      "dist": {str(i): d for d, i in ds[:a.k * 4]}})
    with open(path, "w", encoding="utf-8") as f:
        json.dump(truth, f)
    print(f"ground truth computed in {time.perf_counter() - t0:.1f}s -> {path}", file=sys.stderr)
    return truth


# ---------------------------------------------------------------- database

class Db:
    def __init__(self, a, data_dir, unit, log_path):
        env = ["-E", "MVDB_FORMAT=jsonl"]
        for kv in a.env:
            env += ["-E", kv]
        cmd = (["systemd-run", "--user", "--pipe", "--quiet", "--collect", f"--unit={unit}",
                "-p", "MemoryAccounting=yes", "-p", f"MemoryMax={a.mem_max}", "-p", "MemorySwapMax=0"]
               + env + ["--", a.bin, "--data-dir", data_dir])
        self.unit = unit
        self.log = open(log_path, "w", encoding="utf-8")
        self.t_spawn = time.perf_counter()
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
                                  text=True, encoding="utf-8", bufsize=1)
        ready = self.p.stdout.readline()
        self.t_ready = time.perf_counter()
        if json.loads(ready or "{}").get("ready") is not True:
            self.fail(f"no ready line, got {ready!r}")
        show = subprocess.run(["systemctl", "--user", "show", "-P", "ControlGroup", "-P", "MainPID", f"{unit}.service"],
                              capture_output=True, text=True, check=True).stdout.split()
        self.pid = int(show[0]) if show[0].isdigit() else int(show[1])
        cg = show[1] if show[0].isdigit() else show[0]
        self.cgroup = CGROUP_ROOT + cg

    def fail(self, why):
        self.p.kill()
        self.log.flush()
        with open(self.log.name, encoding="utf-8") as f:
            tail = f.read()[-2000:]
        sys.exit(f"database {self.unit}: {why}\n--- stderr ---\n{tail}")

    def run(self, sql):
        self.p.stdin.write(sql + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            self.fail(f"exited while running {sql[:120]!r}")
        return json.loads(line)

    def mem(self):
        """(memory.peak, memory.current) so far, in bytes."""
        vals = []
        for name in ("memory.peak", "memory.current"):
            with open(os.path.join(self.cgroup, name), encoding="utf-8") as f:
                vals.append(int(f.read().split()[0]))
        return tuple(vals)

    def stats(self):
        def kv(path):
            with open(path, encoding="utf-8") as f:
                return {k: int(v) for k, v in (l.split()[:2] for l in f if len(l.split()) >= 2 and l.split()[1].isdigit())}

        with open(os.path.join(self.cgroup, "memory.peak"), encoding="utf-8") as f:
            peak = int(f.read().split()[0])
        mem = kv(os.path.join(self.cgroup, "memory.stat"))
        cpu = kv(os.path.join(self.cgroup, "cpu.stat"))
        with open(f"/proc/{self.pid}/io", encoding="utf-8") as f:
            io = {k.strip(): int(v) for k, v in (l.split(":") for l in f)}
        return {
            "mem_peak": peak, "mem_anon": mem.get("anon", 0), "mem_file": mem.get("file", 0),
            "read_bytes": io["read_bytes"], "write_bytes": io["write_bytes"],
            "cpu_user_s": cpu.get("user_usec", 0) / 1e6, "cpu_sys_s": cpu.get("system_usec", 0) / 1e6,
        }

    def close(self):
        self.p.stdin.write("quit\n")
        self.p.stdin.close()
        code = self.p.wait(timeout=30)
        self.log.close()
        with open(self.log.name, encoding="utf-8") as f:
            err = f.read()
        if code != 0 or "panicked" in err:
            sys.exit(f"database {self.unit} exited with {code}\n{err[-2000:]}")


def drop_page_cache(data_dir):
    for name in os.listdir(data_dir):
        fd = os.open(os.path.join(data_dir, name), os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


def expect_ok(res, what):
    if "error" in res:
        sys.exit(f"{what}: {res}")


# ---------------------------------------------------------------- phases

def phase_write(a, rows, data_dir, unit):
    if os.path.exists(data_dir):
        shutil.rmtree(data_dir)
    os.makedirs(data_dir)
    db = Db(a, data_dir, unit, os.path.join(a.workdir, f"{unit}.stderr"))
    ready_s = db.t_ready - db.t_spawn
    dim = a.dim
    expect_ok(db.run(f"CREATE TABLE items (id BIGINT PRIMARY KEY, category VARCHAR(32), embedding VECTOR({dim}));"), "create")
    stmts = [insert_sql(a, rows[i:i + a.batch]) for i in range(0, len(rows), a.batch)]
    t0 = time.perf_counter()
    for batch in stmts:
        for sql in batch:
            expect_ok(db.run(sql), "insert")
    t = time.perf_counter() - t0
    st = db.stats()
    count = db.run("SELECT id FROM items;")
    db.close()
    if len(count.get("rows", [])) != len(rows):
        sys.exit(f"write: expected {len(rows)} rows, found {len(count.get('rows', []))}")
    return {"phase": "write", "time_s": t, "ready_s": ready_s, **st}


def phase_query(a, rows, queries, truth, data_dir, unit):
    drop_page_cache(data_dir)
    db = Db(a, data_dir, unit, os.path.join(a.workdir, f"{unit}.stderr"))
    ready_s = db.t_ready - db.t_spawn
    peak_ready = db.mem()[0]
    for cat, v in queries[:a.warmup]:
        expect_ok(db.run(query_sql(a, cat, v)), "warmup")
    peak_warm = db.mem()[0]
    sqls = [query_sql(a, cat, v) for cat, v in queries[a.warmup:]]
    results = []
    t0 = time.perf_counter()
    for sql in sqls:
        results.append(db.run(sql))
        if a.drop_between_queries:
            drop_page_cache(data_dir)
    t = time.perf_counter() - t0
    st = db.stats()
    db.close()

    cat_of = {i: c for i, c, _ in rows}
    recall = {"filtered": [], "unfiltered": []}
    max_rel_err = 0.0
    for (cat, _), res, tr in zip(queries[a.warmup:], results, truth):
        expect_ok(res, "query")
        ids = [r[0] for r in res["rows"]]
        want = min(a.k, tr["n"])
        if len(ids) != want or len(set(ids)) != len(ids):
            sys.exit(f"query returned {ids}, expected {want} distinct ids")
        if cat is not None and any(cat_of.get(i) != cat for i in ids):
            sys.exit(f"query with category={cat} returned {ids}")
        if want == 0:
            continue
        kth = tr["kth"] * (1 + 1e-9)
        hits = sum(1 for i in ids if i in tr["ids"] or tr["dist"].get(str(i), math.inf) <= kth)
        recall["unfiltered" if cat is None else "filtered"].append(hits / want)
        if a.with_distance:
            for i, d in res["rows"]:
                ref = tr["dist"].get(str(i))
                if ref is not None and ref > 0:
                    max_rel_err = max(max_rel_err, abs(d - ref) / ref)
    out = {"phase": "query", "time_s": t, "ready_s": ready_s, **st,
           "mem_peak_at_ready": peak_ready, "mem_peak_after_warmup": peak_warm,
           "recall_filtered": sum(recall["filtered"]) / max(1, len(recall["filtered"])),
           "recall_unfiltered": sum(recall["unfiltered"]) / max(1, len(recall["unfiltered"]))}
    if a.with_distance:
        out["dist_max_rel_err"] = max_rel_err
    return out


def show(r):
    line = (f"{r['phase']:<6} T={r['time_s'] * 1000:8.1f} ms  ready={r['ready_s'] * 1000:6.1f} ms  "
            f"M_peak={r['mem_peak'] / MIB:6.2f} MiB (end: anon {r['mem_anon'] / MIB:.2f}, file {r['mem_file'] / MIB:.2f})  "
            f"B={(r['read_bytes'] + r['write_bytes']) / MIB:7.2f} MiB (r {r['read_bytes'] / MIB:.2f}, w {r['write_bytes'] / MIB:.2f})  "
            f"cpu={r['cpu_user_s'] + r['cpu_sys_s']:.2f}s (sys {r['cpu_sys_s']:.2f})")
    if r["phase"] == "query":
        line += f"  peak@ready={r['mem_peak_at_ready'] / MIB:.2f} peak@warm={r['mem_peak_after_warmup'] / MIB:.2f}"
        line += f"  recall f={r['recall_filtered']:.4f} u={r['recall_unfiltered']:.4f}"
        if "dist_max_rel_err" in r:
            line += f" dist_err={r['dist_max_rel_err']:.1e}"
    print(line)


def check_fresh(binary):
    """Refuses a binary older than the sources it is built from."""
    newest = max(os.path.getmtime(os.path.join(d, f))
                 for d, _, fs in os.walk(os.path.join(ROOT, "src")) for f in fs)
    newest = max([newest] + [os.path.getmtime(os.path.join(ROOT, f)) for f in ("Cargo.toml", "Cargo.lock")])
    if os.path.getmtime(binary) < newest:
        sys.exit(f"{binary} is older than the sources; run cargo build --release")


def main():
    a = parse_args()
    check_fresh(a.bin)
    os.makedirs(a.workdir, exist_ok=True)
    rows, queries = gen_workload(a)
    data_dir = os.path.join(a.workdir, "data")
    tag = f"mvdb-bench-{os.getpid()}"
    rev = subprocess.run(["git", "-C", ROOT, "describe", "--always", "--dirty"], capture_output=True, text=True).stdout.strip()
    out = []
    if a.phase in ("all", "write"):
        out.append(phase_write(a, rows, data_dir, f"{tag}-w"))
    if a.phase in ("all", "query"):
        truth = ground_truth(a, rows, queries)
        out.append(phase_query(a, rows, queries, truth, data_dir, f"{tag}-q"))
    with open(os.path.join(a.workdir, "results.jsonl"), "a", encoding="utf-8") as f:
        for r in out:
            show(r)
            f.write(json.dumps({"label": a.label, "rev": rev, "env": a.env, "style": a.style,
                                "rows": a.rows, "dim": a.dim, "queries": a.queries, **r}) + "\n")


if __name__ == "__main__":
    main()
