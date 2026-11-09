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
| Row cache built on first read; dictionary codes for `=`/`<>` on strings | done, unit-, differential- and crash-tested, measured |
| One statement per input line; semantic error names (`duplicate_key`, ...) | done, as the public cases define them |
| Result rows streamed to stdout (bounded memory for any result size) | done, unit-, differential- and memory-tested |
| Wire format, ready marker, ANN mode switch | waiting for the official test driver |
| Exact vector index (k-means lists and 8-bit codes, triangle-inequality bounds) | done, unit-, differential- and crash-tested, measured |
| Organizers' public sample cases (`mysqltest/`, v4) | 20/20 pass (`tests/public_cases.py`) |
| Approximate (ANN) mode, log compaction | next |

## Testing

```bash
cargo test
cargo build --release
python3 tests/difftest.py --seeds 0:100 --steps 400            # results vs reference model
python3 tests/difftest.py --seeds 0:100 --steps 400 --crash    # plus kill -9 and recovery
MVDB_IVF_MIN_ROWS=1 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash   # vector index on every table
MVDB_IVF_MIN_ROWS=1 MVDB_IVF_MAX_ROWS=30 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash   # tables crossing the list cap
python3 tests/public_cases.py                                   # the organizers' sample cases in mysqltest/
```

`tests/public_cases.py` runs each case listed in `mysqltest/manifest.json`
on a fresh data directory and checks every result against its `.result`
assertions as `mysqltest/README.md` describes them (exact integers, strings
and column names; floats within 1e-5; `unordered` as a multiset; `error` by
its semantic name). A `--restart` line closes stdin, waits for the exit and
starts again on the same directory; `--mode ann` is only noted, since how the
official driver switches modes is not published and exact answers are valid
approximate ones.

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

On the write side, each input line is one statement (as in the public
cases), so nothing scans for `;` across lines; quoted text is skipped eight
bytes at a time when statements are lexed, string literals move from tokens into the
syntax tree instead of being copied, and vector literals are parsed in one
pass. Elements with at most 2^24 as significand and a decimal exponent of at
most 10 (all of the benchmark's) take Clinger's exact fast path: one float32
multiply or divide, bit-identical to `str::parse::<f32>`; anything else goes
through the general parser. What remains of the write phase is mostly the
100 `fdatasync` calls (about 0.8 ms each here).

A table that is written to before anything reads it (a bulk load) keeps no
decoded rows in memory: its row cache is set aside at the first commit and
rebuilt from the log by the first statement that reads the table. Recovery
still decodes rows while it replays the log, since it reads them anyway, so
a database reopened for queries starts with its cache ready.

Median of five runs on an i5-9400F, NVMe ext4:

| Phase | T | Peak memory | Disk I/O |
|---|---|---|---|
| write (100 commits) | 149 ms | 2.1 MiB | 5.45 MiB written |
| query (1,000 TopK, uniform data) | 132 ms | 7.9 MiB | 5.06 MiB read |

Inside the container (`--docker`) `io.stat` reports the same bytes as
`/proc/<pid>/io`, but times are higher: write about 179 ms, query 240-247 ms
(462-552 ms before the vector codes). Each statement's round trip through
Docker's stdio relay costs about 90 µs more (the `--floor` run is 136 ms
instead of 45 ms), which any submission measured this way pays too. The
same binary also uses more CPU per query there, consistent with a native
probe in which idle gaps between statements raise the cost of each query.
Memory there has a floor of its own: an idle container that has only
printed its ready line already shows a `memory.peak` of about 5 MiB while
using 0.5 MiB, so the write phase measures 4.6-4.7 MiB in the container
(7.6 MiB before its row cache was deferred) and query memory is within
0.3 MiB of native.

Tables of 4,096 rows or more also get an exact vector index with two
parts, both exact by the triangle inequality (lists only up to 50,000 rows,
see below):

- **k-means lists.** Rows are grouped into about √n lists around k-means
  centroids, and each list keeps an upper bound on its members' distance to
  the centroid. No member of a list is closer to the query than
  `|q - c| - radius`, so a TopK visits lists in that order and stops at the
  first one whose bound exceeds the K-th distance found so far. When the
  lists are built the cached rows are moved so each list is contiguous,
  which keeps a scan that has to visit every list as fast as the plain one.
- **8-bit codes.** Every component is also stored as
  `c = round(x / step)` in [-127, 127] (one step per column), and every row
  keeps an upper bound on `|x - step * c|`. With the query coded the same
  way, `|q - x| >= step * |cq - cx| - |q - step * cq| - |x - step * cx|`,
  where `|cq - cx|` is computed exactly in integers by an AVX2 kernel. A
  row whose bound exceeds the K-th distance is passed over without reading
  its floats, so most rows cost one byte per component instead of four;
  on uniform data only about 15 rows per query out of 10,000 are left to
  measure in float32.

Results, ties included, are the same as a full scan: every bound carries a
padding far above f64 rounding (and the float32 kernel's proven error bound
where it uses float32 distances), and a row is skipped only when its bound
is strictly greater than the K-th distance. The index is built during
recovery, on `CREATE VECTOR INDEX`, or before a TopK once the table is big
enough or has doubled, never during the write phase; for 10,000 rows it
adds about 60 ms (lists) and 10 ms (codes) to recovery. A filtered TopK
skips the lists while they have recently had to visit more than half the
table, and selects the matching rows directly instead.

Assigning every row to the nearest of about √n centroids makes the lists'
build O(n^1.5), so tables of more than 50,000 rows (`MVDB_IVF_MAX_ROWS`) get
the codes only, and a rebuild past that size drops lists built earlier.
With 128-d uniform rows, recovery of a cached table then takes 181 ms at
100,000 rows and 338 ms at 200,000 (1,112 and 2,259 ms with lists), 843 ms
at 500,000; at 50,000 rows, still with lists, about 475 ms. The price is on
clustered data: at 100,000 rows in 50 clusters of σ = 0.15 a TopK takes
0.92 ms with the codes alone against 0.10 ms with lists (at σ = 0.5 both
about 1 ms).

`=` and `<>` against a constant on a string column compare 4-byte
dictionary codes rather than strings while the column has at most 1,024
distinct values.

Query phase, median of five, recall 1.0 in every run (`MVDB_IVF=0` turns
off the lists, `MVDB_SQ8=0` the codes):

| Data (10,000 x 128) | Neither | Lists | Codes | Both (default) |
|---|---|---|---|---|
| uniform in [-1, 1) | 301 ms | 301 ms | 135 ms | 133 ms |
| 50 clusters, σ = 0.15 | 298 ms | 51 ms | 139 ms | 51 ms |
| 50 clusters, σ = 0.3 | 287 ms | 52 ms | 136 ms | 52 ms |
| 50 clusters, σ = 0.5 | 284 ms | 281 ms | 134 ms | 128 ms |

Peak memory is 6.7-6.8 MiB without the codes and 7.7-7.9 MiB with them.
The lists only prune when clusters are tight relative to the distances
between them: a list is passed over only if `|q - c|` exceeds its radius
plus the K-th distance, and at σ = 0.5 the radius (about 6.7) plus the 10th
distance (7.2) is more than the distance to the nearest other centroid (9.8).
The codes helped on all four shapes, but not on every distribution: on
near-duplicate rows (one vector plus noise), where almost every row is
within the codes' error of the K-th distance, they prune nothing and cost
about 30% (5.5 against 4.2 ms per TopK at 100,000 rows). About 45 ms of each
figure is the benchmark's own round trips (`--floor`).

Environment variables for experiments only: `MVDB_CACHE_BYTES` (row cache
budget; default a quarter of the cgroup memory limit),
`MVDB_DROP_CACHE=0` (keep log pages in the page cache), `MVDB_IVF=0` and
`MVDB_SQ8=0` (leave out the lists or the codes), `MVDB_IVF_MIN_ROWS`
(table size from which the index is used; the differential tests set it
to 1) and `MVDB_IVF_MAX_ROWS` (largest table that gets lists).

## Semantics chosen where the task text is silent

Approved by the student; still to be checked against the official test
package when it is published. The organizers' public cases (`mysqltest/`)
touch none of these; they do confirm that ORDER BY ties go to the smaller
primary key (also under `DESC`), that strings compare by code point, and
that `L2_DISTANCE` is the distance, not its square.

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
`unsupported_sql` error and is never executed with a different meaning.
Expressions nest at most 128 levels (parentheses, function arguments);
deeper input is a `syntax_error`, while AND chains of any length are kept
as one flat list.

## Protocol

Each non-empty input line is one complete statement; a trailing `;` is
optional, an unclosed quote or comment is that line's syntax error, and a
line holding only a comment gets no response. `quit` or `exit` (or end of
input) ends the session; an open transaction is rolled back. Errors carry a
semantic name (`duplicate_key`, `unsupported_sql`, `dimension_mismatch` as
in the public cases; the rest, such as `table_not_found` or
`invalid_value`, are ours until the official mapping is published). Result
rows are written while they are produced: a response is held back until it
passes 64 KiB, then streamed, so `SELECT *` over a table larger than memory
adds no measurable memory (100,000 x 128 rows in a 64 MiB container).
ORDER BY on anything but the primary key still keeps every matching row's
sort key (about 10-25 MiB at 100,000 rows), and TopK its K ids.
`MVDB_FORMAT=jsonl` prints one JSON object per statement; the default text
format is a placeholder until the official driver is published.
