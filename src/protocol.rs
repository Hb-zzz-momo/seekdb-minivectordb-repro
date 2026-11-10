//! The judge's stdin/stdout protocol (STDIO_PROTOCOL.md): one JSON object per
//! line in each direction. A request is
//! `{"id":..,"op":"execute","sql":"..","search_mode":".."}` or
//! `{"id":..,"op":"shutdown"}`, and every request gets exactly one response
//! that repeats its `id`. Everything about the wire format lives here; the
//! engine never writes to stdout directly.

use std::borrow::Cow;
use std::io::{self, Write};

use crate::engine::result::{Cell, Outcome, RowSink};
use crate::error::DbResult;
use crate::sql::lexer::find2;

/// One request line.
#[derive(Debug, PartialEq)]
pub struct Request<'a> {
    /// The `id` value exactly as it was sent, so the response repeats it with
    /// the same JSON type; `null` when the request has none.
    pub id: &'a str,
    pub op: Op<'a>,
}

#[derive(Debug, PartialEq)]
pub enum Op<'a> {
    /// `ann` is set when `search_mode` is `ann`. Vector searches are exact in
    /// both modes: an exact answer meets every requirement on an approximate
    /// one.
    Execute { sql: Cow<'a, str>, ann: bool },
    Shutdown,
    Other(Cow<'a, str>),
}

/// A line that is not a usable request, and the id to answer it under.
#[derive(Debug, PartialEq)]
pub struct BadRequest<'a> {
    pub id: &'a str,
    pub msg: String,
}

#[derive(Default)]
struct Fields<'a> {
    id: Option<&'a str>,
    op: Option<Cow<'a, str>>,
    sql: Option<Cow<'a, str>>,
    mode: Option<Cow<'a, str>>,
}

/// Parses one request line (without its line end).
pub fn parse_request(line: &str) -> Result<Request<'_>, BadRequest<'_>> {
    let mut f = Fields::default();
    let parsed = Json { src: line, pos: 0 }.request(&mut f);
    let id = f.id.unwrap_or("null");
    let bad = |msg: String| BadRequest { id, msg };
    parsed.map_err(bad)?;
    match f.op {
        None => Err(bad("request has no \"op\"".into())),
        Some(op) if op == "execute" => match f.sql {
            Some(sql) => Ok(Request { id, op: Op::Execute { sql, ann: f.mode.as_deref() == Some("ann") } }),
            None => Err(bad("execute request has no \"sql\"".into())),
        },
        Some(op) if op == "shutdown" => Ok(Request { id, op: Op::Shutdown }),
        Some(op) => Ok(Request { id, op: Op::Other(op) }),
    }
}

/// Just enough JSON to read a request: the fields we use are decoded, any
/// other value is skipped whatever its type.
struct Json<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Json<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        let found = self.peek() == Some(c);
        self.pos += found as usize;
        found
    }

    fn request(&mut self, f: &mut Fields<'a>) -> Result<(), String> {
        self.ws();
        if !self.eat(b'{') {
            return Err("a request must be a JSON object".into());
        }
        self.ws();
        if !self.eat(b'}') {
            loop {
                self.ws();
                let key = self.string()?;
                self.ws();
                if !self.eat(b':') {
                    return Err(format!("expected ':' after \"{key}\""));
                }
                self.ws();
                match &*key {
                    "id" => f.id = Some(self.skip()?),
                    "op" => f.op = Some(self.string()?),
                    "sql" => f.sql = Some(self.string()?),
                    "search_mode" if self.peek() == Some(b'"') => f.mode = Some(self.string()?),
                    _ => {
                        self.skip()?;
                    }
                }
                self.ws();
                if self.eat(b'}') {
                    break;
                }
                if !self.eat(b',') {
                    return Err("expected ',' or '}' in the request object".into());
                }
            }
        }
        self.ws();
        if self.pos != self.src.len() {
            return Err("text after the request object".into());
        }
        Ok(())
    }

    /// A string value. One without escapes, the usual case for SQL, is
    /// borrowed from the line.
    fn string(&mut self) -> Result<Cow<'a, str>, String> {
        let b = self.src.as_bytes();
        if self.peek() != Some(b'"') {
            return Err("expected a string".into());
        }
        let start = self.pos + 1;
        let unterminated = || "unterminated string".to_string();
        let first = start + find2(&b[start..], b'"', b'\\').ok_or_else(unterminated)?;
        if b[first] == b'"' {
            self.pos = first + 1;
            return Ok(Cow::Borrowed(&self.src[start..first]));
        }
        let mut out = String::with_capacity(b.len() - start);
        let mut i = start;
        loop {
            // Every cut below is at an ASCII byte, so the slices are valid.
            let stop = i + find2(&b[i..], b'"', b'\\').ok_or_else(unterminated)?;
            out.push_str(&self.src[i..stop]);
            if b[stop] == b'"' {
                self.pos = stop + 1;
                return Ok(Cow::Owned(out));
            }
            let escape = *b.get(stop + 1).ok_or_else(unterminated)?;
            i = stop + 2;
            match escape {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let hi = hex4(b, i)?;
                    i += 4;
                    let c = if (0xD800..0xDC00).contains(&hi) {
                        // The first half of a surrogate pair.
                        match (b.get(i), b.get(i + 1), hex4(b, i + 2)) {
                            (Some(b'\\'), Some(b'u'), Ok(lo)) if (0xDC00..0xE000).contains(&lo) => {
                                i += 6;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            }
                            _ => None,
                        }
                    } else {
                        char::from_u32(hi)
                    };
                    out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                _ => return Err("invalid escape in a string".into()),
            }
        }
    }

    /// Skips one value of any type and returns its text.
    fn skip(&mut self) -> Result<&'a str, String> {
        let start = self.pos;
        match self.peek() {
            None => return Err("unexpected end of the request".into()),
            Some(b'"') => self.skip_string()?,
            Some(b'{' | b'[') => {
                // The open brackets are kept in a list rather than recursed
                // into, so nesting depth costs no stack.
                let mut open = Vec::new();
                loop {
                    match self.peek() {
                        None => return Err("unterminated value".into()),
                        Some(b'"') => {
                            self.skip_string()?;
                            continue;
                        }
                        Some(b'{') => open.push(b'}'),
                        Some(b'[') => open.push(b']'),
                        Some(c @ (b'}' | b']')) if open.pop() != Some(c) => {
                            return Err("mismatched brackets in the request".into());
                        }
                        _ => {}
                    }
                    self.pos += 1;
                    if open.is_empty() {
                        break;
                    }
                }
            }
            Some(_) => {
                while self.peek().is_some_and(|c| !matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')) {
                    self.pos += 1;
                }
                let text = &self.src[start..self.pos];
                if !matches!(text, "true" | "false" | "null") && !is_number(text) {
                    return Err("invalid value in the request".into());
                }
            }
        }
        Ok(&self.src[start..self.pos])
    }

    fn skip_string(&mut self) -> Result<(), String> {
        let b = self.src.as_bytes();
        let mut i = self.pos + 1;
        loop {
            i += find2(&b[i.min(b.len())..], b'"', b'\\').ok_or("unterminated string")?;
            if b[i] == b'"' {
                self.pos = i + 1;
                return Ok(());
            }
            i += 2;
        }
    }
}

fn hex4(b: &[u8], at: usize) -> Result<u32, String> {
    let digits = b.get(at..at + 4).ok_or("truncated \\u escape")?;
    digits.iter().try_fold(0u32, |v, &d| Some(v * 16 + (d as char).to_digit(16)?)).ok_or_else(|| "invalid \\u escape".into())
}

/// `-? digits [. digits] [(e|E) [+-] digits]`, the JSON number form.
fn is_number(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |mut i: usize| {
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        (i > start).then_some(i)
    };
    let Some(mut i) = digits((b.first() == Some(&b'-')) as usize) else {
        return false;
    };
    if b.get(i) == Some(&b'.') {
        match digits(i + 1) {
            Some(j) => i = j,
            None => return false,
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let sign = matches!(b.get(i + 1), Some(b'+' | b'-')) as usize;
        match digits(i + 1 + sign) {
            Some(j) => i = j,
            None => return false,
        }
    }
    i == b.len()
}

/// The line a freshly started database sends once recovery is complete.
pub fn write_ready(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"{\"type\":\"ready\"}\n")
}

/// Success without a result, as for `shutdown`.
pub fn write_ok(out: &mut impl Write, id: &str) -> io::Result<()> {
    writeln!(out, "{{\"id\":{id},\"ok\":true}}")
}

/// Failure of a request that never reached the engine.
pub fn write_error(out: &mut impl Write, id: &str, code: &str, msg: &str) -> io::Result<()> {
    let mut line = format!("{{\"id\":{id},\"ok\":false,");
    push_error(&mut line, code, msg);
    line.push_str("}\n");
    out.write_all(line.as_bytes())
}

/// Bytes of a response held back before rows start going out.
const SPILL: usize = 64 << 10;

/// One statement's response. Rows are formatted into a buffer; a result
/// that stays under `SPILL` bytes is sent as
/// `{"id":..,"ok":true,"columns":[..],"rows":[..]}`. A larger one goes out
/// while it is produced, so it needs bounded memory, and carries `ok` at the
/// end of the object instead: a statement that fails after rows went out (an
/// I/O error reading the table) can then still answer `"ok":false`.
pub struct Response<'a, W: Write> {
    out: &'a mut W,
    id: &'a str,
    /// `"columns":[..],"rows":[` and the rows not written yet.
    buf: String,
    columns: bool,
    rows: u64,
    spilled: bool,
}

impl<'a, W: Write> Response<'a, W> {
    pub fn new(out: &'a mut W, id: &'a str) -> Self {
        Response { out, id, buf: String::new(), columns: false, rows: 0, spilled: false }
    }

    /// Completes the response with the statement's outcome.
    pub fn finish(mut self, res: DbResult<Outcome>) -> io::Result<()> {
        let id = self.id;
        let line = match res {
            Ok(Outcome::Rows) => {
                if !self.columns {
                    let _ = self.columns(&[]);
                }
                if self.spilled {
                    self.buf.push_str("],\"ok\":true}\n");
                    self.buf
                } else {
                    format!("{{\"id\":{id},\"ok\":true,{}]}}\n", self.buf)
                }
            }
            Ok(Outcome::Affected(n)) => format!("{{\"id\":{id},\"ok\":true,\"affected_rows\":{n}}}\n"),
            Ok(Outcome::Done) => format!("{{\"id\":{id},\"ok\":true}}\n"),
            Err(e) => {
                // After rows went out: close their array and add the error.
                let mut line = if self.spilled { self.buf + "]," } else { format!("{{\"id\":{id},") };
                line.push_str("\"ok\":false,");
                push_error(&mut line, e.kind.name(), &e.msg);
                line.push_str("}\n");
                line
            }
        };
        self.out.write_all(line.as_bytes())
    }
}

impl<W: Write> RowSink for Response<'_, W> {
    fn columns(&mut self, names: &[String]) -> DbResult<()> {
        self.buf.push_str("\"columns\":[");
        for (i, c) in names.iter().enumerate() {
            if i > 0 {
                self.buf.push(',');
            }
            push_json_str(&mut self.buf, c);
        }
        self.buf.push_str("],\"rows\":[");
        self.columns = true;
        Ok(())
    }

    fn row(&mut self, cells: &[Cell]) -> DbResult<()> {
        if self.rows > 0 {
            self.buf.push(',');
        }
        self.buf.push('[');
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                self.buf.push(',');
            }
            push_cell(&mut self.buf, cell);
        }
        self.buf.push(']');
        self.rows += 1;
        if self.buf.len() >= SPILL {
            if !self.spilled {
                write!(self.out, "{{\"id\":{},", self.id)?;
                self.spilled = true;
            }
            self.out.write_all(self.buf.as_bytes())?;
            self.buf.clear();
        }
        Ok(())
    }
}

fn push_error(s: &mut String, code: &str, msg: &str) {
    s.push_str("\"error\":{\"code\":");
    push_json_str(s, code);
    s.push_str(",\"message\":");
    push_json_str(s, msg);
    s.push('}');
}

fn push_cell(s: &mut String, cell: &Cell) {
    use std::fmt::Write as _;
    match cell {
        Cell::Int(v) => {
            let _ = write!(s, "{v}");
        }
        // `{:?}` gives the shortest text that reads back as the same value
        // and always has a `.` or an exponent, so a float stays a float in
        // JSON (5.0, not 5). Values are finite; `null` is only a guard.
        Cell::Float(v) if v.is_finite() => {
            let _ = write!(s, "{v:?}");
        }
        Cell::Float(_) => s.push_str("null"),
        Cell::Str(v) => push_json_str(s, v),
        Cell::Vector(v) => {
            s.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "{x:?}");
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

    fn execute<'a>(id: &'a str, sql: &'a str, ann: bool) -> Result<Request<'a>, BadRequest<'a>> {
        Ok(Request { id, op: Op::Execute { sql: Cow::Borrowed(sql), ann } })
    }

    #[test]
    fn requests() {
        assert_eq!(
            parse_request(r#"{"id":1,"op":"execute","sql":"SELECT id FROM docs ORDER BY id;","search_mode":"exact"}"#),
            execute("1", "SELECT id FROM docs ORDER BY id;", false)
        );
        // Any key order, spacing and extra fields; the id keeps its text.
        assert_eq!(
            parse_request(r#" { "search_mode" : "ann", "x": [1, {"a": "}\""}], "sql" : "SELECT 1", "op":"execute", "id": "a-7" } "#),
            execute("\"a-7\"", "SELECT 1", true)
        );
        assert_eq!(parse_request(r#"{"id":4,"op":"shutdown"}"#), Ok(Request { id: "4", op: Op::Shutdown }));
        assert_eq!(parse_request(r#"{"id":1.5e3,"op":"ping"}"#), Ok(Request { id: "1.5e3", op: Op::Other("ping".into()) }));
        assert_eq!(parse_request(r#"{"op":"shutdown","id":null}"#), Ok(Request { id: "null", op: Op::Shutdown }));
        assert_eq!(parse_request(r#"{"op":"shutdown"}"#), Ok(Request { id: "null", op: Op::Shutdown }));
        // A string with a semicolon and non-ASCII text is one statement.
        assert_eq!(
            parse_request(r#"{"id":2,"op":"execute","sql":"INSERT INTO t VALUES (1, 'a;b 你好')"}"#),
            execute("2", "INSERT INTO t VALUES (1, 'a;b 你好')", false)
        );
    }

    #[test]
    fn escapes_in_sql() {
        let r = parse_request(r#"{"id":3,"op":"execute","sql":"SELECT \"a\\b\"\n\t-- 你好 😀 \/ \ud800x"}"#).unwrap();
        let Op::Execute { sql, .. } = r.op else { panic!() };
        assert_eq!(sql, "SELECT \"a\\b\"\n\t-- 你好 😀 / \u{fffd}x");
    }

    #[test]
    fn bad_requests_keep_their_id() {
        for (line, id) in [
            ("SELECT 1;", "null"),
            ("", "null"),
            ("[1]", "null"),
            (r#"{"id":7,"op":"execute"}"#, "7"),
            (r#"{"id":7}"#, "7"),
            (r#"{"id":7,"op":"execute","sql":5}"#, "7"),
            (r#"{"id":7,"op":"execute","sql":"x"} trailing"#, "7"),
            (r#"{"id":7,"op":"execute","sql":"unterminated}"#, "7"),
            (r#"{"id":7,"op":"execute","sql":"bad \q escape"}"#, "7"),
            (r#"{"id":abc,"op":"shutdown"}"#, "null"),
            (r#"{"id":[1,"op":"shutdown"}"#, "null"),
        ] {
            assert_eq!(parse_request(line).unwrap_err().id, id, "{line}");
        }
    }

    #[test]
    fn numbers() {
        for ok in ["0", "-1", "12", "1.5", "1e9", "-2.5E-3", "9007199254740993"] {
            assert!(is_number(ok), "{ok}");
        }
        for bad in ["", "-", "1.", ".5", "1e", "1e+", "abc", "1x", "+1"] {
            assert!(!is_number(bad), "{bad}");
        }
    }

    fn respond(id: &str, rows: usize, res: DbResult<Outcome>) -> String {
        let mut out = Vec::new();
        let mut r = Response::new(&mut out, id);
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
    fn responses() {
        assert_eq!(respond("2", 0, Ok(Outcome::Affected(3))), "{\"id\":2,\"ok\":true,\"affected_rows\":3}\n");
        assert_eq!(respond("\"x\"", 0, Ok(Outcome::Done)), "{\"id\":\"x\",\"ok\":true}\n");
        assert_eq!(respond("1", 0, Ok(Outcome::Rows)), "{\"id\":1,\"ok\":true,\"columns\":[\"id\",\"v\"],\"rows\":[]}\n");
        assert_eq!(
            respond("1", 2, Ok(Outcome::Rows)),
            "{\"id\":1,\"ok\":true,\"columns\":[\"id\",\"v\"],\"rows\":[[0,\"a\\\"b\"],[1,\"a\\\"b\"]]}\n"
        );
        let err = || Err(db_err!(DuplicateKey, "dup \"1\""));
        let want = "{\"id\":3,\"ok\":false,\"error\":{\"code\":\"duplicate_key\",\"message\":\"dup \\\"1\\\"\"}}\n";
        assert_eq!(respond("3", 0, err()), want);
        // An error after a few rows that were never written replaces them.
        assert_eq!(respond("3", 3, err()), want);

        let mut out = Vec::new();
        write_ready(&mut out).unwrap();
        write_ok(&mut out, "4").unwrap();
        write_error(&mut out, "null", "invalid_request", "no").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"ready\"}\n{\"id\":4,\"ok\":true}\n{\"id\":null,\"ok\":false,\"error\":{\"code\":\"invalid_request\",\"message\":\"no\"}}\n"
        );
    }

    #[test]
    fn cells() {
        let mut s = String::new();
        for cell in [
            Cell::Int(-9007199254740993),
            Cell::Float(5.0),
            Cell::Float(0.1),
            Cell::Float(1e-7),
            Cell::Float(3.4028234663852886e38),
            Cell::Float(-0.0),
            Cell::Float(f64::INFINITY),
            Cell::Str("你\n".into()),
            Cell::Vector(vec![3.0, 0.5, 1e-7, f32::MAX]),
        ] {
            push_cell(&mut s, &cell);
            s.push(' ');
        }
        assert_eq!(
            s,
            "-9007199254740993 5.0 0.1 1e-7 3.4028234663852886e38 -0.0 null \"你\\n\" [3.0,0.5,1e-7,3.4028235e38] "
        );
    }

    #[test]
    fn large_results_stream_and_stay_one_object() {
        let n = 20_000;
        let line = respond("9", n, Ok(Outcome::Rows));
        assert!(line.starts_with("{\"id\":9,\"columns\":[\"id\",\"v\"],\"rows\":[[0,"));
        assert!(line.ends_with("]],\"ok\":true}\n") && line.lines().count() == 1);
        assert_eq!(line.matches('[').count(), n + 2);
        // Rows already went out, so the failure closes the same object.
        let failed = respond("9", n, Err(db_err!(Io, "disk")));
        assert!(failed.len() >= SPILL && failed.lines().count() == 1);
        assert!(failed.ends_with("]],\"ok\":false,\"error\":{\"code\":\"io_error\",\"message\":\"disk\"}}\n"));
    }
}
