//! stdin/stdout wire protocol. Everything driver-specific lives here: statement
//! boundaries, result-set and error formatting, the ready marker and the exit
//! command. The text format is a placeholder until the official test driver
//! is published; the JSON-lines format is for our own test harness. The
//! engine never writes to stdout directly.

use std::io::{self, Write};

use crate::engine::result::{Cell, ExecResult};
use crate::error::DbResult;
use crate::sql::lexer::find2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    /// One JSON object per statement: `{"ok":true,"affected":n}`,
    /// `{"columns":[..],"rows":[[..]]}` or `{"error":"Kind","message":".."}`.
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

/// `quit` / `exit` end the session normally.
pub fn is_exit(stmt: &str) -> bool {
    stmt.eq_ignore_ascii_case("quit") || stmt.eq_ignore_ascii_case("exit")
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
        Err(e) => writeln!(out, "ERROR {:?}: {}", e.kind, e.msg),
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
            s.push_str(&format!("{{\"error\":\"{:?}\",\"message\":", e.kind));
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

/// Splits an input stream into statements at `;` outside quotes and comments.
#[derive(Default)]
pub struct Splitter {
    buf: String,
    /// Byte offset in `buf` where scanning resumes.
    scan: usize,
    state: State,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Normal,
    Quote(u8),
    LineComment,
    BlockComment,
}

impl Splitter {
    /// Feeds more input and appends every completed statement (without its
    /// terminating `;`, trimmed, non-empty) to `out`.
    pub fn push(&mut self, text: &str, out: &mut Vec<String>) {
        self.buf.push_str(text);
        let b = self.buf.as_bytes();
        let mut cuts = Vec::new();
        let mut i = self.scan;
        while i < b.len() {
            let c = b[i];
            match self.state {
                State::Normal => match c {
                    b'\'' | b'"' | b'`' => self.state = State::Quote(c),
                    b'#' => self.state = State::LineComment,
                    b'-' if b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|x| x.is_ascii_whitespace()) => {
                        self.state = State::LineComment
                    }
                    b'/' if b.get(i + 1) == Some(&b'*') => {
                        self.state = State::BlockComment;
                        i += 1;
                    }
                    b';' => cuts.push(i),
                    _ => {}
                },
                State::Quote(q) => {
                    // Jump to the closing quote or the next escape.
                    let esc = if q == b'`' { q } else { b'\\' };
                    match find2(&b[i..], q, esc) {
                        Some(off) if b[i + off] == q => {
                            i += off;
                            self.state = State::Normal;
                        }
                        Some(off) => i += off + 1,
                        None => i = b.len() - 1,
                    }
                }
                State::LineComment => {
                    if c == b'\n' {
                        self.state = State::Normal;
                    }
                }
                State::BlockComment => {
                    if c == b'*' && b.get(i + 1) == Some(&b'/') {
                        self.state = State::Normal;
                        i += 1;
                    }
                }
            }
            i += 1;
        }
        let mut start = 0;
        for cut in cuts {
            let stmt = self.buf[start..cut].trim();
            if !stmt.is_empty() {
                out.push(stmt.to_string());
            }
            start = cut + 1;
        }
        self.buf.drain(..start);
        self.scan = i - start;
    }

    /// Returns trailing input that was never terminated by `;`.
    pub fn finish(&mut self) -> Option<String> {
        let rest = self.buf.trim().to_string();
        self.buf.clear();
        self.scan = 0;
        self.state = State::Normal;
        (!rest.is_empty()).then_some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&str]) -> Vec<String> {
        let mut s = Splitter::default();
        let mut out = Vec::new();
        for c in chunks {
            s.push(c, &mut out);
        }
        out.extend(s.finish());
        out
    }

    #[test]
    fn splits_outside_quotes_and_comments() {
        assert_eq!(
            split(&["SELECT 'a;b', \"c;\", `d;` FROM t; -- x;y\n", "INSERT INTO t VALUES (1, 'it''s;');", "/* ; */ COMMIT;"]),
            vec![
                "SELECT 'a;b', \"c;\", `d;` FROM t",
                "-- x;y\nINSERT INTO t VALUES (1, 'it''s;')",
                "/* ; */ COMMIT",
            ]
        );
    }

    #[test]
    fn statements_span_chunks() {
        assert_eq!(split(&["SELECT *\n", "FROM t\n", "WHERE a = 'x\\';y';\n", "BEGIN"]), vec![
            "SELECT *\nFROM t\nWHERE a = 'x\\';y'",
            "BEGIN"
        ]);
        assert_eq!(split(&[";;", " ; "]), Vec::<String>::new());
    }
}
