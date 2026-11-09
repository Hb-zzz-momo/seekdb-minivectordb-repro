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
| Executor, transactions, PK index | next |
| Wire format, error codes, ready marker | waiting for the official test driver |
| Vector index (ANN) | after correctness |

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
