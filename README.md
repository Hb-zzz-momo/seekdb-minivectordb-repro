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
| Docker image (pinned bases, `tests/docker_smoke.sh`) | builds, smoke-tested, benchmarked |
| Write path: single-pass vector parsing, no re-lexing | done, unit- and differential-tested, measured |
| Wire format, error codes, ready marker | waiting for the official test driver |
| Exact vector index (k-means lists, triangle-inequality pruning) | done, unit-, differential- and crash-tested, measured |
| Approximate (ANN) mode, log compaction | next |

## Testing

```bash
cargo test
cargo build --release
python3 tests/difftest.py --seeds 0:100 --steps 400            # results vs reference model
python3 tests/difftest.py --seeds 0:100 --steps 400 --crash    # plus kill -9 and recovery
MVDB_IVF_MIN_ROWS=1 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash   # vector index on every table
```

`tests/difftest.py` generates random sessions (DDL, multi-row INSERT with
duplicate keys and invalid values, UPDATE, DELETE, filtered SELECT, TopK,
BEGIN/COMMIT/ROLLBACK) and compares every result with a reference model
written in Python. In `--crash` mode it also kills the database at random
points, including while a commit is in flight and while recovery is running,
and checks the recovered contents equal the model's committed state. Set
`MVDB_FORMAT=jsonl` to get the same machine-readable output by hand.

```bash
docker build -t minivectordb:smoke .
tests/docker_smoke.sh minivectordb:smoke
```

The smoke test runs the image the way the grader does (`docker run -i`, a
volume at `/data`): statements, a rolled-back transaction, a filtered TopK,
data surviving a restart, an acknowledged commit surviving `docker kill`, and
a clean exit on `quit` and at end of input.

## Performance

```bash
cargo build --release
python3 bench/bench.py                               # write phase, then query phase
python3 bench/bench.py --env MVDB_DROP_CACHE=0       # same, without page-cache dropping
python3 bench/bench.py --docker minivectordb:smoke   # inside the image, with io.stat
python3 bench/bench.py --phase query --floor         # same queries, scan removed
python3 bench/bench.py --clusters 50 --sigma 0.15    # Gaussian clusters instead of uniform data
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

On the write side, quoted text is skipped eight bytes at a time when
statements are split and lexed, string literals move from tokens into the
syntax tree instead of being copied, and vector literals are parsed in one
pass. Elements with at most 2^24 as significand and a decimal exponent of at
most 10 (all of the benchmark's) take Clinger's exact fast path: one float32
multiply or divide, bit-identical to `str::parse::<f32>`; anything else goes
through the general parser. What remains of the write phase is mostly the
100 `fdatasync` calls (about 0.8 ms each here).

Median of five runs on an i5-9400F, NVMe ext4:

| Phase | T | Peak memory | Disk I/O |
|---|---|---|---|
| write (100 commits) | 151 ms | 7.3 MiB | 5.45 MiB written |
| query (1,000 TopK) | 311 ms | 6.8 MiB | 5.06 MiB read |

Inside the container (`--docker`) `io.stat` reports the same bytes as
`/proc/<pid>/io`, and memory is within 0.3 MiB, but times are higher: write
170 ms (377 ms before the write-path work), query 462-552 ms. Each statement's round trip through Docker's stdio
relay costs about 90 µs more (the `--floor` run is 136 ms instead of 45 ms),
which any submission measured this way pays too. The same binary also uses
more CPU per query there (0.35 s instead of 0.29 s), consistent with a native
probe in which idle gaps between statements raise the cost of each query.

Tables of 4,096 rows or more also get an exact vector index. Rows are
grouped into about √n lists around k-means centroids, and each list keeps
an upper bound on its members' distance to the centroid. By the triangle
inequality no member of a list is closer to the query than
`|q - c| - radius`, so a TopK visits lists in that order and stops at the
first one whose bound exceeds the K-th distance found so far; results,
ties included, are the same as a full scan. The bound uses the float32
kernel's proven error bound plus a padding far above f64 rounding. When the
index is built (during recovery, on `CREATE VECTOR INDEX`, or before a TopK
once the table is big enough or has doubled), the cached rows are moved so
each list is contiguous, which keeps a scan that has to visit every list as
fast as the plain one. It is never built during the write phase; building
it adds about 60 ms to recovery for 10,000 rows.

Query phase, median of five, recall 1.0 in every case, peak memory
unchanged (6.6-6.8 MiB):

| Data (10,000 x 128) | Without index | With index |
|---|---|---|
| uniform in [-1, 1) | 305 ms | 315 ms |
| 50 clusters, σ = 0.15 | 310 ms | 50 ms |
| 50 clusters, σ = 0.3 | 289 ms | 51 ms |
| 50 clusters, σ = 0.5 | 287 ms | 305 ms |

The bounds only prune when clusters are tight relative to the distances
between them; on uniform data or wide clusters the index visits every row.

Environment variables for experiments only: `MVDB_CACHE_BYTES` (row cache
budget; default a quarter of the cgroup memory limit) and
`MVDB_DROP_CACHE=0` (keep log pages in the page cache), `MVDB_IVF=0` (no
vector index) and `MVDB_IVF_MIN_ROWS` (table size from which the index is
used; the differential tests set it to 1).

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
