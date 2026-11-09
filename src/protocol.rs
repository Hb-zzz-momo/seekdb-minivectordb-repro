//! stdin/stdout wire protocol. Everything driver-specific lives here: statement
//! boundaries, result-set and error formatting, the ready marker and the exit
//! command. Formats are placeholders until the official test driver is
//! published; the engine never writes to stdout directly.

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
                    if c == b'\\' && q != b'`' {
                        i += 1;
                    } else if c == q {
                        self.state = State::Normal;
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
