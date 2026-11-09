use std::io::{self, BufRead, BufWriter, Write};

use minivectordb::protocol::Splitter;
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
    eprintln!("minivectordb: data dir {data_dir} (executor not wired yet: statements are only parsed)");

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = BufWriter::new(io::stdout().lock());
    let mut splitter = Splitter::default();
    let mut stmts = Vec::new();
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
        splitter.push(&String::from_utf8_lossy(&raw), &mut stmts);
        for s in stmts.drain(..) {
            run(&s, &mut out);
        }
        let _ = out.flush();
    }
    if let Some(s) = splitter.finish() {
        run(&s, &mut out);
    }
    let _ = out.flush();
}

/// Placeholder until the executor lands: report whether the statement parses.
fn run(sql_text: &str, out: &mut impl Write) {
    let _ = match sql::parse(sql_text) {
        Ok(stmt) => writeln!(out, "PARSED {stmt:?}"),
        Err(e) => writeln!(out, "ERROR {e}"),
    };
}
