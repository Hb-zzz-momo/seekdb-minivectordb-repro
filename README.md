# MiniVectorDB

A single-process vector database for the OceanBase 2026 tiebreaker task: SQL over
stdin/stdout, durable transactions, crash recovery and exact/approximate
`L2_DISTANCE` TopK search. Written in Rust with no third-party dependencies.

## Build and run

```bash
cargo build --release
./target/release/minivectordb --data-dir ./data      # SQL on stdin

docker build -t minivectordb .
docker run -i --rm -v mvdb-data:/data minivectordb   # -i only: no -t, no -d
```

The data directory defaults to `/data` and can be changed with `--data-dir` or
`MVDB_DATA_DIR`. Logs go to stderr; stdout carries only results.

## Status

| Part | State |
|---|---|
| Lexer and parser for the SQL subset | done, unit-tested |
| Value types, vector literal validation (NaN/Inf/float32 overflow) | done |
| Append-only commit log with torn-tail recovery | done, crash-tested |
| Executor: catalog, PK index, row cache, exact TopK, transactions | done, differential-tested |
| AVX2/FMA distance prefilter (exact results), benchmark harness | done, measured |
| Columnar row cache with filter pushdown | done, differential-tested, measured |
| Wire format, error codes, ready marker | waiting for the official test driver |
| Vector index (ANN), log compaction | next |

## Testing

```bash
cargo test
cargo build --release
python3 tests/difftest.py --seeds 0:100 --steps 400            # results vs reference model
python3 tests/difftest.py --seeds 0:100 --steps 400 --crash    # plus kill -9 and recovery
```

`tests/difftest.py` generates random sessions (DDL, multi-row INSERT with
duplicate keys and invalid values, UPDATE, DELETE, filtered SELECT, TopK,
BEGIN/COMMIT/ROLLBACK) and compares every result with a reference model
written in Python. In `--crash` mode it also kills the database at random
points, including while a commit is in flight and while recovery is running,
and checks the recovered contents equal the model's committed state. Set
`MVDB_FORMAT=jsonl` to get the same machine-readable output by hand.

## Performance

```bash
cargo build --release
python3 bench/bench.py                               # write phase, then query phase
python3 bench/bench.py --env MVDB_DROP_CACHE=0       # same, without page-cache dropping
```

`bench/bench.py` follows the task's benchmark: 10,000 rows of 128-d vectors
inserted 100 per commit, then a cold restart and 1,000 TopK queries (K = 10,
every second one with an equality filter). Each phase runs in its own
transient systemd unit, so `memory.peak` is read from a real cgroup and
includes page cache, as on the platform. This machine's user systemd has no
io controller, so disk I/O comes from `/proc/<pid>/io` instead of
`io.stat`. Query results are checked for count, duplicates, the filter and
Recall@10 against distances computed independently in Python. Every run
appends to `target/bench/results.jsonl`.

While a table fits the memory budget its rows are cached column by column:
one array per column (strings back to back in one buffer, vectors packed),
indexed by slot. TopK and ORDER BY queries walk the slots directly instead
of the primary-key index, and `column <op> constant` conditions run as
loops over their column arrays first, so only matching rows are looked at.
Each surviving row is measured with a float32 AVX2/FMA kernel (picked at run
time, scalar fallback otherwise), and its exact f64 distance is computed
only when a proven error bound says it could still place, so results and
distances are the same as a full exact scan.

Median of five runs on an i5-9400F, NVMe ext4:

| Phase | T | Peak memory | Disk I/O |
|---|---|---|---|
| write (100 commits) | 292 ms | 7.6 MiB | 5.45 MiB written |
| query (1,000 TopK) | 311 ms | 6.8 MiB | 5.06 MiB read |

Environment variables for experiments only: `MVDB_CACHE_BYTES` (row cache
budget; default a quarter of the cgroup memory limit) and
`MVDB_DROP_CACHE=0` (keep log pages in the page cache).

## Semantics chosen where the task text is silent

Approved by the student; still to be checked against the official test
package when it is published:

- DDL and `BEGIN` implicitly commit an open transaction (as in MySQL);
  `COMMIT`/`ROLLBACK` outside a transaction are no-ops.
- `UPDATE ... SET` values are validated before any row is matched, so a bad
  value is an error even when no row matches.
- Comparing a number with a string, or storing a number in a VARCHAR/VECTOR
  column (or a string in BIGINT), is a type error rather than an implicit
  conversion. A decimal literal stored in BIGINT must be integral.
- Rows without ORDER BY come back in primary-key order.
- Table and column names are case-insensitive.

## Storage design

The commit log is the only copy of the data (`/data/seg-<id>.log`). Each
committed transaction is one frame `[len][crc32c][lsn, ops...]`, written with a
single `pwrite` and made durable with `fdatasync` before the commit is
acknowledged. Recovery replays valid frames and truncates the newest segment at
the first torn frame, so a crash leaves each transaction fully applied or
absent, and recovery itself can be interrupted and rerun safely. Written pages
are dropped from the page cache after the sync, because page cache counts
toward the container's measured memory.

## SQL subset

`CREATE TABLE` (BIGINT, VARCHAR(n), VECTOR(n); one BIGINT primary key, at most
one vector column), `DROP TABLE`, `CREATE VECTOR INDEX`, multi-row `INSERT`,
`SELECT` with `=`/`<>`/`<`/`<=`/`>`/`>=`/`AND`, `ORDER BY`, `LIMIT`,
`L2_DISTANCE`, `UPDATE`, `DELETE`, `BEGIN`/`COMMIT`/`ROLLBACK`. Other SQL
(OR, NOT, NULL, JOIN, aggregates, subqueries, ALTER, ...) returns an
`Unsupported` error and is never executed with a different meaning.
