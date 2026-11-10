# MiniVectorDB

A single-process vector database for the OceanBase 2026 tiebreaker task: SQL
requests over stdin/stdout (the judge's JSON protocol, `STDIO_PROTOCOL.md`),
durable transactions, crash recovery and exact `L2_DISTANCE` TopK search.
Written in Rust 1.98.1 with no third-party dependencies.

## Build and run

```bash
cargo build --release
./target/release/minivectordb --data-dir ./data      # JSON requests on stdin

docker build -t minivectordb .
docker run -i --rm -v mvdb-data:/data minivectordb   # -i only: no -t, no -d
```

The image's entry point is `minivectordb --data-dir /data`. The data
directory defaults to `/data` and can be changed with `--data-dir` or
`MVDB_DATA_DIR`; nothing is written anywhere else, so the image runs with a
read-only root file system. Logs go to stderr; stdout carries only protocol
lines. A session from an empty directory is shown under [Protocol](#protocol).

## Status

| Part | State |
|---|---|
| Lexer and parser for the SQL subset | done, unit-tested |
| Value types, vector literal validation (NaN/Inf/float32 overflow) | done |
| Append-only commit log with torn-tail recovery | done, crash-tested |
| UPDATE and DELETE larger than the memory limit (frames written and replayed in pieces) | done, unit-, differential-, crash- and memory-tested |
| Executor: catalog, PK index, row cache, exact TopK, transactions | done, differential-tested |
| AVX2/FMA distance prefilter (exact results), benchmark harness | done, measured |
| Columnar row cache with filter pushdown | done, differential-tested, measured |
| Docker image (pinned bases, `tests/docker_smoke.sh`) | builds, smoke-tested, benchmarked |
| Write path: single-pass vector parsing, no re-lexing | done, unit- and differential-tested, measured |
| Row cache built on first read; dictionary codes for `=`/`<>` on strings | done, unit-, differential- and crash-tested, measured |
| Semantic error codes (`duplicate_key`, `unsupported_sql`, ...) | done, as the public cases define them |
| Result rows streamed to stdout (bounded memory for any result size) | done, unit-, differential- and memory-tested |
| The judge's JSON protocol: ready line, `execute`, `shutdown`, `search_mode` | done; the organizers' checker passes on the image (handshake and 20/20 public cases) |
| Exact vector index (k-means lists and 8-bit codes, triangle-inequality bounds) | done, unit-, differential- and crash-tested, measured |
| Organizers' public sample cases (`mysqltest/`, v4) | 20/20 pass through `protocol_check/run.py` |
| Polling for the next query instead of sleeping (off under a CPU quota below 1) | done, measured through the organizers' transport |
| Approximate (ANN) mode, log compaction | next |

## Testing

```bash
cargo test
cargo build --release
python3 tests/difftest.py --seeds 0:100 --steps 400            # results vs reference model
python3 tests/difftest.py --seeds 0:100 --steps 400 --crash    # plus kill -9 and recovery
MVDB_IVF_MIN_ROWS=1 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash   # vector index on every table
MVDB_IVF_MIN_ROWS=1 MVDB_IVF_MAX_ROWS=30 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash   # tables crossing the list cap
MVDB_CACHE_BYTES=3000 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash                 # rows read from the log
MVDB_LOG_CHUNK=64 MVDB_BIG_FRAME=0 python3 tests/difftest.py --seeds 0:100 --steps 400 --crash    # frames written in pieces, replayed op by op
python3 tests/mem_probe.py                                      # statements larger than the memory limit, in a 64 MiB cgroup
python3 tests/public_cases.py                                   # the organizers' sample cases in mysqltest/
```

`tests/public_cases.py` runs the organizers' own checker,
`protocol_check/run.py`, against the release binary: it waits for the ready
line, sends every statement of every case in `mysqltest/manifest.json` as a
JSON request, restarts on `--restart` (a `shutdown` request, then a new
process on the same directory), sets `search_mode` to `ann` after
`--mode ann`, and compares each response with the case's `.result`
assertions as the judge does (exact integers, strings and column names;
floats within 1e-5; `unordered` as a multiset; a non-empty `error.code`).

`tests/difftest.py` generates random sessions (DDL, multi-row INSERT with
duplicate keys and invalid values, UPDATE, DELETE, filtered SELECT, TopK,
BEGIN/COMMIT/ROLLBACK) and compares every result with a reference model
written in Python. In `--crash` mode it also kills the database at random
points, including while a commit is in flight and while recovery is running,
and checks the recovered contents equal the model's committed state. It
speaks the judge's protocol, and every normal restart is a `shutdown`
request that must be acknowledged and followed by exit status 0.

```bash
docker build -t minivectordb:smoke .
tests/docker_smoke.sh minivectordb:smoke
```

The smoke test runs the image with the judge's container options
(`docker run -i`, no network, 2 CPUs, 2 GiB, a read-only root file system, no
capabilities, a volume at `/data`): the organizers' checker (handshake and
all public cases), then statements, a rolled-back transaction, a filtered
TopK, data surviving a restart, an acknowledged commit surviving
`docker kill`, a clean exit at end of input, and `docker stop` returning at
once with exit code 0.

## Performance

```bash
cargo build --release
python3 bench/bench.py                               # write phase, then query phase
python3 bench/bench.py --env MVDB_DROP_CACHE=0       # same, without page-cache dropping
python3 bench/bench.py --docker minivectordb:smoke --cpus 2   # inside the image, with io.stat
python3 bench/bench.py --phase query --floor         # same queries, scan removed
python3 bench/bench.py --clusters 50 --sigma 0.15    # Gaussian clusters instead of uniform data
```

`bench/bench.py` follows the task's benchmark: 10,000 rows of 128-d vectors
inserted 100 per commit, then a cold restart and 1,000 TopK queries (K = 10,
every second one with an equality filter). It speaks the judge's protocol,
and like the judge it encodes each JSON request inside the timed loop: on
this machine that encoding alone is 67 ms of the write phase and 20 ms of
the query phase (with the requests encoded beforehand the phases take 134
and 125 ms), a cost every submission carries. Each phase runs in its own
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
rebuilt from the log by the first statement that reads the table (or by
`CREATE VECTOR INDEX`). The rebuild reads the log front to back, as recovery
does, dropping pages behind itself: with the load and the queries in one
process the peak is 8.2 MiB, against 13.3 MiB when the rows were fetched one
by one, which left the whole log in the page cache. `MVDB_DEFER_CACHE=0`
keeps the rows in memory during the load instead (load peak 7.0 MiB, and no
second read of the log). Recovery
still decodes rows while it replays the log, since it reads them anyway, so
a database reopened for queries starts with its cache ready.

Medians on an i5-9400F, NVMe ext4, through the judge's protocol (the first
four rows with `bench/bench.py`, five runs; the last two with the organizers'
own transport, `protocol_check/_transport.py`, and their container options,
ten runs):

| Phase | T | Peak memory | Disk I/O |
|---|---|---|---|
| write (100 commits) | 204 ms | 1.7 MiB | 5.45 MiB written |
| query (1,000 TopK, uniform data) | 137 ms | 7.9 MiB | 5.06 MiB read |
| write, in the image (`--docker --cpus 2`) | 221-225 ms | 4.4-5.4 MiB | 5.45 MiB written |
| query, in the image | 230 ms | 8.1 MiB | 5.06 MiB read |
| write, organizers' transport | 214 ms | | |
| query, organizers' transport | 285 ms | | |

After answering a query the database polls standard input for up to 1 ms
before it sleeps in `read`, so the next query of a client that sends them
back to back is picked up by a process that is still running. Through the
organizers' transport, in random order with the version before it, the 1,000
queries take 285.3 ms instead of 302.5 (-5.7%) with their 2 CPUs and 290.3
instead of 309.1 (-6.1%) with 1; natively `bench.py` gives 137 instead of
147 ms. The window is 1 ms because their client sends the next query about
180 us after an answer: a first window of 200 us missed most of them (307 ms),
while 500, 1,000 and 2,000 us gave 288 to 290 ms. Polling occupies a CPU, so
it is on only without a CPU quota or with one of at least 1 CPU (read from
`cpu.max`; with `--cpus=0.5` it is off), and it stops for good if the
process is ever descheduled for 2 ms while polling. Without the first check
a scratch build made the query phase 43 to 61% slower under half a CPU. It
follows queries only: during the write phase the process polls once, after
the last statement, and through the organizers' transport the write phase is
unchanged (214.2 against 216.4 ms). `bench.py --docker` does show the write
phase about 2% slower with polling on, but only when it passes the container
no environment variable at all, also not an unrelated one; that was not
explained and did not reproduce with the organizers' transport or with a
minimal client.

Inside the container `io.stat` reports the same bytes as `/proc/<pid>/io`,
but times are higher: each statement's round trip through
Docker's stdio relay costs about 90 µs more (measured before the protocol
change: a `--floor` run of 136 ms
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
recovery, by `CREATE VECTOR INDEX` (which first loads the rows of a table
that was only written to so far: 78 ms for 10,000 rows, after which the
first TopK takes 0.2 ms), or before a TopK once the table is big
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
off the lists, `MVDB_SQ8=0` the codes). These were measured before the
harness spoke the judge's protocol, so each lacks the roughly 20 ms of
request encoding that the figures above include:

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
budget; default a quarter of the memory limit of the process's cgroup or of
any group above it, read from `/proc/self/cgroup` and `/sys/fs/cgroup`),
`MVDB_DROP_CACHE=0` (keep log pages in the page cache),
`MVDB_DEFER_CACHE=0` (keep a table's rows in memory while it is only written
to), `MVDB_SPIN_US` (microseconds of polling for the next request after a
query, default 1000; 0 turns it off), `MVDB_LOG_CHUNK` and `MVDB_BIG_FRAME` (bytes after which a frame is
written in pieces, and replayed without being held in memory; the tests set
them to 64 and 0), `MVDB_IVF=0` and
`MVDB_SQ8=0` (leave out the lists or the codes), `MVDB_IVF_MIN_ROWS`
(table size from which the index is used; the differential tests set it
to 1) and `MVDB_IVF_MAX_ROWS` (largest table that gets lists).

## Semantics chosen where the task text is silent

Decided by the student; the official error-code mapping and the hidden
cases are still unpublished. The organizers' public cases (`mysqltest/`)
touch none of these; they do confirm that ORDER BY ties go to the smaller
primary key (also under `DESC`), that strings compare by code point, and
that `L2_DISTANCE` is the distance, not its square.

- `BEGIN` inside a transaction is an `unsupported_sql` error and leaves the
  transaction open: the task lists nested transactions among the unsupported
  features, and unsupported statements must fail rather than run with
  another meaning. DDL implicitly commits an open transaction (as in MySQL);
  `COMMIT`/`ROLLBACK` outside a transaction succeed and do nothing.
- `UPDATE` reports the rows its WHERE clause matched, whether or not their
  values changed: the expected results come from the organizers' reference
  executor, not from MySQL, whose default would count only changed rows.
- `UPDATE ... SET` values are validated before any row is matched, so a bad
  value is an error even when no row matches.
- Comparing a number with a string, or storing a number in a VARCHAR/VECTOR
  column (or a string in BIGINT), is a type error rather than an implicit
  conversion. A decimal literal stored in BIGINT must be integral.
- Rows without ORDER BY come back in primary-key order.
- Table and column names are case-insensitive.

The first two are one-line switches (`Semantics` in `src/engine/mod.rs`),
also settable for a test run with `MVDB_NESTED_BEGIN=error|commit` and
`MVDB_UPDATE_COUNT=matched|changed`; `tests/difftest.py --nested-begin` and
`--update-count` check the engine against the model under either choice.

## Storage design

The commit log is the only copy of the data (`/data/seg-<id>.log`). Each
committed transaction is one frame `[len][crc32c][lsn, ops...]`, written with a
single `pwrite` and made durable with `fdatasync` before the commit is
acknowledged. Recovery replays valid frames and truncates the newest segment at
the first torn frame, so a crash leaves each transaction fully applied or
absent, and recovery itself can be interrupted and rerun safely. Written pages
are dropped from the page cache after the sync, because page cache counts
toward the container's measured memory.

A frame larger than 1 MiB is written in pieces behind a header that is still
zero, which recovery reads as a torn tail; the header is written last, then
the frame is synced. Recovery checks the sum of a frame larger than 4 MiB in
one pass and applies its operations in a second, without holding it in
memory. With that, an UPDATE or DELETE outside a transaction needs a key and a
location per row it changes (24 bytes) besides one buffer: it collects the
keys, then reads, changes and writes one row at a time, and applies the
change once the frame is durable. In a 64 MiB container, `UPDATE` of all
100,000 rows of a 51 MiB table succeeds and is replayed by the next start
(`tests/mem_probe.py`); before, the statement held every row about 4.6 times
over and was killed.

Known limits: an explicit transaction is held in memory until COMMIT: in
the same 64 MiB container one that updates 50,000 rows (28 MiB) commits and
one that updates 70,000 (39 MiB) is killed; the primary-key index takes
about 55 bytes per row whatever the cache budget; ORDER BY on a column other
than the primary key keeps every matching row's sort key; the log is never
compacted, so overwritten and deleted rows stay on disk and are skipped by
each start.

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

The judge's protocol, as `STDIO_PROTOCOL.md` defines it: UTF-8 JSON Lines,
one object per line in each direction, flushed after every line.

- After recovery the database prints `{"type":"ready"}`, before it reads
  anything.
- `{"id":N,"op":"execute","sql":"...","search_mode":"exact"}` runs one
  statement (a trailing `;` is optional; the SQL may span lines). The answer
  repeats `id` exactly as it was sent, whatever its JSON type:
  - a query: `{"id":N,"ok":true,"columns":[...],"rows":[[...],...]}`;
  - INSERT, UPDATE, DELETE: `{"id":N,"ok":true,"affected_rows":K}`;
  - DDL, BEGIN, COMMIT, ROLLBACK: `{"id":N,"ok":true}`;
  - a failure: `{"id":N,"ok":false,"error":{"code":"...","message":"..."}}`,
    after which the session continues.
- `{"id":N,"op":"shutdown"}` rolls back an open transaction, answers
  `{"id":N,"ok":true}` and exits with status 0. End of input without it,
  SIGTERM (`docker stop`) and SIGINT also end the session with status 0 and
  the open transaction rolled back.
- `search_mode` may be `exact` or `ann`. Vector searches are exact in both:
  an exact answer satisfies every check on an approximate one.
- BIGINT values are JSON integers, distances and vector elements are JSON
  numbers that always carry a `.` or an exponent (`5.0`), strings are JSON
  strings.
- Error codes: `duplicate_key`, `unsupported_sql`, `dimension_mismatch` (the
  three the public cases name), `syntax_error`, `table_not_found`,
  `table_exists`, `column_not_found`, `column_count_mismatch`,
  `missing_value`, `type_mismatch`, `out_of_range`, `data_too_long`,
  `invalid_value`, `invalid_schema`, `io_error`, `corrupt_data`,
  `internal_error`; `invalid_request` and `unsupported_op` for a line that
  is not a usable request. The official mapping is not frozen; the judge
  currently requires only a non-empty code.
- A result set is held back until it passes 64 KiB, then written while it is
  produced, with `"ok"` at the end of the object instead of the front, so a
  result of any size needs bounded memory. ORDER BY on anything but the
  primary key still keeps every matching row's sort key, and TopK its K ids.

A session from an empty data directory (`>` is a request, `<` a response):

```text
< {"type":"ready"}
> {"id":1,"op":"execute","sql":"CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR(32), content VARCHAR(1024), embedding VECTOR(4));","search_mode":"exact"}
< {"id":1,"ok":true}
> {"id":2,"op":"execute","sql":"INSERT INTO docs VALUES (1, 'database', '事务与恢复', '[1, 0, 0, 0]'), (2, 'database', '向量索引', '[0, 1, 0, 0]'), (3, 'travel', '旅行攻略', '[0, 0, 1, 0]');","search_mode":"exact"}
< {"id":2,"ok":true,"affected_rows":3}
> {"id":3,"op":"execute","sql":"SELECT id, content FROM docs WHERE id >= 2 ORDER BY id;","search_mode":"exact"}
< {"id":3,"ok":true,"columns":["id","content"],"rows":[[2,"向量索引"],[3,"旅行攻略"]]}
> {"id":4,"op":"execute","sql":"SELECT id, L2_DISTANCE(embedding, '[0.9, 0.1, 0, 0]') AS distance FROM docs WHERE category = 'database' ORDER BY distance LIMIT 2;","search_mode":"exact"}
< {"id":4,"ok":true,"columns":["id","distance"],"rows":[[1,0.141421374149721],[2,1.272792188223375]]}
> {"id":5,"op":"execute","sql":"BEGIN;","search_mode":"exact"}
< {"id":5,"ok":true}
> {"id":6,"op":"execute","sql":"UPDATE docs SET embedding = '[0.8, 0.2, 0, 0]' WHERE id = 1;","search_mode":"exact"}
< {"id":6,"ok":true,"affected_rows":1}
> {"id":7,"op":"execute","sql":"COMMIT;","search_mode":"exact"}
< {"id":7,"ok":true}
> {"id":8,"op":"execute","sql":"BEGIN;","search_mode":"exact"}
< {"id":8,"ok":true}
> {"id":9,"op":"execute","sql":"DELETE FROM docs WHERE id = 2;","search_mode":"exact"}
< {"id":9,"ok":true,"affected_rows":1}
> {"id":10,"op":"execute","sql":"ROLLBACK;","search_mode":"exact"}
< {"id":10,"ok":true}
> {"id":11,"op":"execute","sql":"INSERT INTO docs VALUES (1, 'dup', 'x', '[0, 0, 0, 0]');","search_mode":"exact"}
< {"id":11,"ok":false,"error":{"code":"duplicate_key","message":"duplicate entry '1' for key 'PRIMARY'"}}
> {"id":12,"op":"shutdown"}
< {"id":12,"ok":true}
```

and after a restart on the same directory:

```text
< {"type":"ready"}
> {"id":1,"op":"execute","sql":"SELECT id, L2_DISTANCE(embedding, '[0.9, 0.1, 0, 0]') AS distance FROM docs ORDER BY distance LIMIT 3;","search_mode":"exact"}
< {"id":1,"ok":true,"columns":["id","distance"],"rows":[[1,0.14142133200287407],[2,1.272792188223375],[3,1.3490737405281743]]}
> {"id":2,"op":"shutdown"}
< {"id":2,"ok":true}
```
