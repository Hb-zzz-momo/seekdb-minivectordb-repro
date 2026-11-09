//! stdin/stdout wire protocol. Everything driver-specific lives here: statement
//! boundaries, result-set and error formatting, the ready marker and the exit
//! command. The text format is a placeholder until the official test driver
//! is published; the JSON-lines format is for our own test harness. The
//! engine never writes to stdout directly.

use std::io::{self, Write};

use crate::engine::result::{Cell, ExecResult};
use crate::error::DbResult;
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

pub fn write_result(out: &mut impl Write, fmt: Format, res: &DbResult<ExecResult>) -> io::Result<()> {
    match fmt {
        Format::Jsonl => write_jsonl(out, res),
        Format::Text => write_text(out, res),
    }
}

fn write_text(out: &mut impl Write, res: &DbResult<ExecResult>) -> io::Result<()> {
    match res {
        Ok(ExecResult::Ok { affected }) => writeln!(out, "OK {affected}"),
        Ok(ExecResult::Rows { columns, rows }) => {
            writeln!(out, "{}", columns.join("\t"))?;
            let mut line = String::new();
            for row in rows {
                line.clear();
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    push_cell(&mut line, cell, false);
                }
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
        Err(e) => writeln!(out, "ERROR {}: {}", e.kind.name(), e.msg),
    }
}

fn write_jsonl(out: &mut impl Write, res: &DbResult<ExecResult>) -> io::Result<()> {
    let mut s = String::new();
    match res {
        Ok(ExecResult::Ok { affected }) => s.push_str(&format!("{{\"ok\":true,\"affected\":{affected}}}")),
        Ok(ExecResult::Rows { columns, rows }) => {
            s.push_str("{\"columns\":[");
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                push_json_str(&mut s, c);
            }
            s.push_str("],\"rows\":[");
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push('[');
                for (j, cell) in row.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    push_cell(&mut s, cell, true);
                }
                s.push(']');
            }
            s.push_str("]}");
        }
        Err(e) => {
            s.push_str(&format!("{{\"error\":\"{}\",\"message\":", e.kind.name()));
            push_json_str(&mut s, &e.msg);
            s.push('}');
        }
    }
    writeln!(out, "{s}")
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

    #[test]
    fn one_statement_per_line() {
        assert_eq!(statement("  SELECT 'a;b' FROM t; \r\n"), Some("SELECT 'a;b' FROM t;"));
        assert_eq!(statement("INSERT INTO t VALUES (1, 'unclosed\n"), Some("INSERT INTO t VALUES (1, 'unclosed"));
        assert_eq!(statement(" -- note\n"), None);
        assert_eq!(statement("\n"), None);
        assert!(is_exit("quit;") && is_exit("EXIT") && !is_exit("quit_it"));
    }
}
