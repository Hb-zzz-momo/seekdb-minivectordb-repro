use std::io::{self, BufRead, BufWriter, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use minivectordb::db_err;
use minivectordb::engine::result::ExecResult;
use minivectordb::engine::{Engine, EngineOptions};
use minivectordb::error::DbResult;
use minivectordb::protocol::{self, Format, Splitter};
use minivectordb::sql;

fn main() {
    let mut data_dir = std::env::var("MVDB_DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--data-dir" {
            if let Some(d) = args.next() {
                data_dir = d;
            }
        }
    }
    let fmt = Format::from_env();

    // Recovery (log replay, cache rebuild) completes before we report ready.
    let mut engine = match Engine::open(Path::new(&data_dir), EngineOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("minivectordb: cannot open database in {data_dir}: {e}");
            std::process::exit(1);
        }
    };
    let mut out = BufWriter::new(io::stdout().lock());
    let _ = protocol::write_ready(&mut out, fmt);
    let _ = out.flush();

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut splitter = Splitter::default();
    let mut stmts = Vec::new();
    let mut raw = Vec::new();
    'session: loop {
        raw.clear();
        match input.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("minivectordb: stdin: {e}");
                break;
            }
        }
        splitter.push(&String::from_utf8_lossy(&raw), &mut stmts);
        for text in stmts.drain(..) {
            if protocol::is_exit(&text) {
                break 'session;
            }
            if sql::parser::is_blank(&text) {
                continue;
            }
            let res = run(&mut engine, &text);
            let _ = protocol::write_result(&mut out, fmt, &res);
            let _ = out.flush();
        }
    }
    if let Some(text) = splitter.finish() {
        if !protocol::is_exit(&text) && !sql::parser::is_blank(&text) {
            let res = run(&mut engine, &text);
            let _ = protocol::write_result(&mut out, fmt, &res);
        }
    }
    let _ = out.flush();
    // Dropping the engine discards any open transaction; every committed one
    // is already durable.
}

/// Parses and executes one statement. A panic is reported as an error and
/// rolls back the open transaction instead of taking the process down.
fn run(engine: &mut Engine, text: &str) -> DbResult<ExecResult> {
    let stmt = sql::parse(text)?;
    match panic::catch_unwind(AssertUnwindSafe(|| engine.execute(&stmt))) {
        Ok(res) => res,
        Err(_) => {
            engine.abort_transaction();
            Err(db_err!(Internal, "internal error; the transaction was rolled back"))
        }
    }
}
