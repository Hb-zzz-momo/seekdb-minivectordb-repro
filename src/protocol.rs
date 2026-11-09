//! stdin/stdout wire protocol. Everything driver-specific lives here: statement
//! boundaries, result-set and error formatting, the ready marker and the exit
//! command. The text format is a placeholder until the official test driver
//! is published; the JSON-lines format is for our own test harness. The
//! engine never writes to stdout directly.

use std::io::{self, Write};

use crate::engine::result::{Cell, Outcome, RowSink};
use crate::error::{DbError, DbResult};
use crate::sql::parser::is_blank;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    /// One JSON object per statement: `{"ok":true,"affected":n}`,
    /// `{"columns":[..],"rows":[[..]]}` or `{"error":"name","message":".."}`.
    Jsonl,
}

impl Format {
    /// From `MVDB_FORMAT` (`jsonl` or `text`).
    pub fn from_env() -> Format {
        match std::env::var("MVDB_FORMAT").as_deref() {
            Ok("jsonl") => Format::Jsonl,
            _ => Format::Text,
        }
    }
}

/// The statement on one input line, or `None` for a line of only whitespace
/// and comments. Each non-empty line is one complete statement (the public
/// cases' rule), so an unclosed quote or a missing `;` is that line's syntax
/// error rather than a reason to wait for more input.
pub fn statement(line: &str) -> Option<&str> {
    let s = line.trim();
    (!is_blank(s)).then_some(s)
}

/// `quit` / `exit` end the session normally.
pub fn is_exit(stmt: &str) -> bool {
    let s = stmt.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    s.eq_ignore_ascii_case("quit") || s.eq_ignore_ascii_case("exit")
}

pub fn write_ready(out: &mut impl Write, fmt: Format) -> io::Result<()> {
    match fmt {
        Format::Jsonl => writeln!(out, "{{\"ready\":true}}"),
        Format::Text => Ok(()),
    }
}

/// Bytes of a response held back before rows start going out.
const SPILL: usize = 64 << 10;

/// One statement's response. Rows are formatted into a buffer that is
/// written out whenever it passes `SPILL` bytes, so a result set of any size
/// needs bounded memory. A statement that fails before anything was written
/// gets a plain error response; one that fails after rows went out (an I/O
/// error reading the table) gets the error appended to them.
pub struct Response<'a, W: Write> {
    out: &'a mut W,
    fmt: Format,
    buf: String,
    columns: bool,
    rows: u64,
    spilled: bool,
}

impl<'a, W: Write> Response<'a, W> {
    pub fn new(out: &'a mut W, fmt: Format) -> Self {
        Response { out, fmt, buf: String::new(), columns: false, rows: 0, spilled: false }
    }

    /// Completes the response with the statement's outcome.
    pub fn finish(mut self, res: DbResult<Outcome>) -> io::Result<()> {
        match res {
            Ok(Outcome::Rows) => {
                if !self.columns {
                    let _ = self.columns(&[]);
                }
                if self.fmt == Format::Jsonl {
                    self.buf.push_str("]}\n");
                }
            }
            Ok(Outcome::Affected(n)) => {
                self.buf.clear();
                match self.fmt {
                    Format::Jsonl => self.buf.push_str(&format!("{{\"ok\":true,\"affected\":{n}}}\n")),
                    Format::Text => self.buf.push_str(&format!("OK {n}\n")),
                }
            }
            Err(e) => {
                if !self.spilled {
                    self.buf.clear();
                }
                self.error(&e);
            }
        }
        self.out.write_all(self.buf.as_bytes())
    }

    fn error(&mut self, e: &DbError) {
        match self.fmt {
            Format::Jsonl => {
                // After rows went out: close their array and add the error.
                let head = if self.spilled { "]," } else { "{" };
                self.buf.push_str(&format!("{head}\"error\":\"{}\",\"message\":", e.kind.name()));
                push_json_str(&mut self.buf, &e.msg);
                self.buf.push_str("}\n");
            }
            Format::Text => self.buf.push_str(&format!("ERROR {}: {}\n", e.kind.name(), e.msg)),
        }
    }
}

impl<W: Write> RowSink for Response<'_, W> {
    fn columns(&mut self, names: &[String]) -> DbResult<()> {
        match self.fmt {
            Format::Jsonl => {
                self.buf.push_str("{\"columns\":[");
                for (i, c) in names.iter().enumerate() {
                    if i > 0 {
                        self.buf.push(',');
                    }
                    push_json_str(&mut self.buf, c);
                }
                self.buf.push_str("],\"rows\":[");
            }
            Format::Text => {
                self.buf.push_str(&names.join("\t"));
                self.buf.push('\n');
            }
        }
        self.columns = true;
        Ok(())
    }

    fn row(&mut self, cells: &[Cell]) -> DbResult<()> {
        let json = self.fmt == Format::Jsonl;
        if json {
            if self.rows > 0 {
                self.buf.push(',');
            }
            self.buf.push('[');
        }
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                self.buf.push(if json { ',' } else { '\t' });
            }
            push_cell(&mut self.buf, cell, json);
        }
        self.buf.push(if json { ']' } else { '\n' });
        self.rows += 1;
        if self.buf.len() >= SPILL {
            self.out.write_all(self.buf.as_bytes())?;
            self.buf.clear();
            self.spilled = true;
        }
        Ok(())
    }
}

fn push_cell(s: &mut String, cell: &Cell, json: bool) {
    use std::fmt::Write as _;
    match cell {
        Cell::Int(v) => {
            let _ = write!(s, "{v}");
        }
        Cell::Float(v) => {
            let _ = write!(s, "{v}");
        }
        Cell::Str(v) if json => push_json_str(s, v),
        Cell::Str(v) => s.push_str(v),
        Cell::Vector(v) => {
            s.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "{x}");
            }
            s.push(']');
        }
    }
}

fn push_json_str(s: &mut String, v: &str) {
    use std::fmt::Write as _;
    s.push('"');
    for ch in v.chars() {
        match ch {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_err;

    fn respond(fmt: Format, rows: usize, res: DbResult<Outcome>) -> String {
        let mut out = Vec::new();
        let mut r = Response::new(&mut out, fmt);
        if matches!(res, Ok(Outcome::Rows)) || rows > 0 {
            r.columns(&["id".into(), "v".into()]).unwrap();
        }
        for i in 0..rows {
            r.row(&[Cell::Int(i as i64), Cell::Str("a\"b".into())]).unwrap();
        }
        r.finish(res).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn one_statement_per_line() {
        assert_eq!(statement("  SELECT 'a;b' FROM t; \r\n"), Some("SELECT 'a;b' FROM t;"));
        assert_eq!(statement("INSERT INTO t VALUES (1, 'unclosed\n"), Some("INSERT INTO t VALUES (1, 'unclosed"));
        assert_eq!(statement(" -- note\n"), None);
        assert_eq!(statement("\n"), None);
        assert!(is_exit("quit;") && is_exit("EXIT") && !is_exit("quit_it"));
    }

    #[test]
    fn responses() {
        assert_eq!(respond(Format::Jsonl, 0, Ok(Outcome::Affected(3))), "{\"ok\":true,\"affected\":3}\n");
        assert_eq!(
            respond(Format::Jsonl, 2, Ok(Outcome::Rows)),
            "{\"columns\":[\"id\",\"v\"],\"rows\":[[0,\"a\\\"b\"],[1,\"a\\\"b\"]]}\n"
        );
        assert_eq!(respond(Format::Text, 1, Ok(Outcome::Rows)), "id\tv\n0\ta\"b\n");
        let err = || Err(db_err!(DuplicateKey, "dup"));
        assert_eq!(respond(Format::Jsonl, 0, err()), "{\"error\":\"duplicate_key\",\"message\":\"dup\"}\n");
        // An error after a few rows that were never written replaces them.
        assert_eq!(respond(Format::Jsonl, 3, err()), "{\"error\":\"duplicate_key\",\"message\":\"dup\"}\n");
        assert_eq!(respond(Format::Text, 3, err()), "ERROR duplicate_key: dup\n");
    }

    #[test]
    fn large_results_stream_and_stay_valid_json() {
        let n = 20_000;
        let line = respond(Format::Jsonl, n, Ok(Outcome::Rows));
        let v: Vec<&str> = line.matches("[").collect();
        assert_eq!(v.len(), n + 2);
        assert!(line.ends_with("]]}\n") && line.lines().count() == 1);
        // Rows already went out, so the error is appended to them.
        let failed = respond(Format::Jsonl, n, Err(db_err!(Io, "disk")));
        assert!(failed.len() >= SPILL && failed.ends_with("],\"error\":\"io_error\",\"message\":\"disk\"}\n"));
    }
}
