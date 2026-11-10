use std::io::{self, BufRead, BufWriter, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use minivectordb::db_err;
use minivectordb::engine::result::{Outcome, RowSink};
use minivectordb::engine::{Engine, EngineOptions};
use minivectordb::error::DbResult;
use minivectordb::protocol::{self, Op, Request, Response};
use minivectordb::sql;

fn main() {
    exit_on_termination_signals();
    // `--data-dir`, else MVDB_DATA_DIR, else the variable the organizers'
    // checker sets for a local run, else /data.
    let mut data_dir = std::env::var("MVDB_DATA_DIR")
        .or_else(|_| std::env::var("VECTORDB_DATA_DIR"))
        .unwrap_or_else(|_| "/data".to_string());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            if let Some(d) = args.next() {
                data_dir = d;
            }
        }
    }

    // Recovery (log replay, cache rebuild) completes before we report ready.
    let mut engine = match Engine::open(Path::new(&data_dir), EngineOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("minivectordb: cannot open database in {data_dir}: {e}");
            std::process::exit(1);
        }
    };
    let mut out = BufWriter::new(io::stdout().lock());
    let _ = protocol::write_ready(&mut out);
    let _ = out.flush();

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match input.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("minivectordb: stdin: {e}");
                break;
            }
        }
        // One request per line, one response per request.
        let line = match std::str::from_utf8(&raw) {
            Ok(line) => line.trim(),
            Err(_) => {
                let _ = protocol::write_error(&mut out, "null", "invalid_request", "request is not valid UTF-8");
                let _ = out.flush();
                continue;
            }
        };
        if line.is_empty() {
            continue;
        }
        let _ = match protocol::parse_request(line) {
            Ok(Request { id, op: Op::Execute { sql, .. } }) => {
                let mut response = Response::new(&mut out, id);
                let res = run(&mut engine, &sql, &mut response);
                response.finish(res)
            }
            Ok(Request { id, op: Op::Shutdown }) => {
                // An open transaction is rolled back; every committed one is
                // already durable. Nothing may follow the acknowledgement.
                engine.abort_transaction();
                let _ = protocol::write_ok(&mut out, id);
                let _ = out.flush();
                return;
            }
            Ok(Request { id, op: Op::Other(op) }) => {
                protocol::write_error(&mut out, id, "unsupported_op", &format!("unknown op '{op}'"))
            }
            Err(bad) => protocol::write_error(&mut out, bad.id, "invalid_request", &bad.msg),
        };
        let _ = out.flush();
    }
    // End of input without a shutdown request: dropping the engine discards
    // any open transaction; every committed one is already durable.
}

/// SIGTERM and SIGINT end the session like end of input. Every acknowledged
/// commit is already durable and an open transaction exists only in memory,
/// so leaving at once is a normal shutdown that rolls it back; a frame cut
/// off by the exit is a torn tail, which recovery discards. The handler is
/// needed for `docker stop` to work at all: the database is process 1 in its
/// container, and the kernel delivers SIGTERM to process 1 only if it has
/// installed a handler.
fn exit_on_termination_signals() {
    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        fn _exit(code: i32) -> !;
    }
    extern "C" fn on_signal(_: i32) {
        // SAFETY: _exit is async-signal-safe.
        unsafe { _exit(0) }
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    // SAFETY: the handler only calls an async-signal-safe function.
    unsafe {
        signal(SIGTERM, on_signal);
        signal(SIGINT, on_signal);
    }
}

/// Parses and executes one statement. A panic is reported as an error and
/// rolls back the open transaction instead of taking the process down.
fn run(engine: &mut Engine, text: &str, sink: &mut dyn RowSink) -> DbResult<Outcome> {
    match panic::catch_unwind(AssertUnwindSafe(|| engine.execute_into(&sql::parse(text)?, sink))) {
        Ok(res) => res,
        Err(_) => {
            engine.abort_transaction();
            Err(db_err!(Internal, "internal error; the transaction was rolled back"))
        }
    }
}
