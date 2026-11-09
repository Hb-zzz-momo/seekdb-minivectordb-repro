use crate::error::DbResult;
use crate::{bail, db_err};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Unquoted identifier or keyword; keywords are matched case-insensitively.
    Ident(String),
    /// Backquoted identifier.
    QuotedIdent(String),
    /// Raw numeric literal text, e.g. `42`, `1.5`, `2e-3`.
    Number(String),
    /// String literal with escapes resolved.
    Str(String),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Slash,
    Percent,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    /// Byte span in the source text.
    pub start: usize,
    pub end: usize,
}

pub fn tokenize(src: &str) -> DbResult<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|x| x.is_ascii_whitespace())) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let close = src[i + 2..].find("*/").ok_or_else(|| db_err!(Syntax, "unterminated comment"))?;
            i += 2 + close + 2;
            continue;
        }
        let start = i;
        let next = b.get(i + 1).copied();
        let tok = match c {
            b'(' => single(&mut i, Tok::LParen),
            b')' => single(&mut i, Tok::RParen),
            b',' => single(&mut i, Tok::Comma),
            b';' => single(&mut i, Tok::Semicolon),
            b'*' => single(&mut i, Tok::Star),
            b'+' => single(&mut i, Tok::Plus),
            b'-' => single(&mut i, Tok::Minus),
            b'/' => single(&mut i, Tok::Slash),
            b'%' => single(&mut i, Tok::Percent),
            b'=' => single(&mut i, Tok::Eq),
            b'<' => match next {
                Some(b'=') => double(&mut i, Tok::Le),
                Some(b'>') => double(&mut i, Tok::Ne),
                _ => single(&mut i, Tok::Lt),
            },
            b'>' => match next {
                Some(b'=') => double(&mut i, Tok::Ge),
                _ => single(&mut i, Tok::Gt),
            },
            b'!' if next == Some(b'=') => double(&mut i, Tok::Ne),
            b'\'' | b'"' => {
                let (s, end) = lex_string(src, i, c)?;
                i = end;
                Tok::Str(s)
            }
            b'`' => {
                let (s, end) = lex_quoted_ident(src, i)?;
                i = end;
                Tok::QuotedIdent(s)
            }
            b'0'..=b'9' => {
                i = lex_number(b, i);
                Tok::Number(src[start..i].to_string())
            }
            b'.' if next.is_some_and(|x| x.is_ascii_digit()) => {
                i = lex_number(b, i);
                Tok::Number(src[start..i].to_string())
            }
            b'.' => single(&mut i, Tok::Dot),
            _ if is_ident_byte(c) => {
                while i < b.len() && (is_ident_byte(b[i]) || b[i].is_ascii_digit()) {
                    i += 1;
                }
                Tok::Ident(src[start..i].to_string())
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or('?');
                bail!(Syntax, "unexpected character '{}' at position {}", ch, i);
            }
        };
        out.push(Token { tok, start, end: i });
    }
    out.push(Token { tok: Tok::Eof, start: b.len(), end: b.len() });
    Ok(out)
}

fn single(i: &mut usize, t: Tok) -> Tok {
    *i += 1;
    t
}

fn double(i: &mut usize, t: Tok) -> Tok {
    *i += 2;
    t
}

/// Letters, `_`, `$` and any non-ASCII byte (UTF-8 identifiers).
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn lex_number(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    i
}

/// MySQL-style string: doubled quote or backslash escapes.
fn lex_string(src: &str, start: usize, quote: u8) -> DbResult<(String, usize)> {
    let b = src.as_bytes();
    let mut buf = Vec::new();
    let mut i = start + 1;
    loop {
        let Some(&c) = b.get(i) else {
            bail!(Syntax, "unterminated string literal");
        };
        if c == quote {
            if b.get(i + 1) == Some(&quote) {
                buf.push(quote);
                i += 2;
                continue;
            }
            i += 1;
            break;
        }
        if c == b'\\' && i + 1 < b.len() {
            let e = b[i + 1];
            match e {
                b'0' => buf.push(0),
                b'n' => buf.push(b'\n'),
                b'r' => buf.push(b'\r'),
                b't' => buf.push(b'\t'),
                b'b' => buf.push(8),
                b'Z' => buf.push(26),
                // MySQL keeps the backslash for LIKE wildcards.
                b'%' | b'_' => buf.extend_from_slice(&[b'\\', e]),
                _ => buf.push(e),
            }
            i += 2;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    let s = String::from_utf8(buf).map_err(|_| db_err!(Syntax, "invalid UTF-8 in string literal"))?;
    Ok((s, i))
}

fn lex_quoted_ident(src: &str, start: usize) -> DbResult<(String, usize)> {
    let b = src.as_bytes();
    let mut s = String::new();
    let mut i = start + 1;
    let mut seg = i;
    loop {
        match b.get(i) {
            None => bail!(Syntax, "unterminated quoted identifier"),
            Some(b'`') if b.get(i + 1) == Some(&b'`') => {
                s.push_str(&src[seg..=i]);
                i += 2;
                seg = i;
            }
            Some(b'`') => {
                s.push_str(&src[seg..i]);
                break;
            }
            Some(_) => i += 1,
        }
    }
    if s.is_empty() {
        bail!(Syntax, "empty quoted identifier");
    }
    Ok((s, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        tokenize(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            toks("SELECT id, `my col` FROM t WHERE a<=1.5e3 AND b<>'x''y';"),
            vec![
                Tok::Ident("SELECT".into()),
                Tok::Ident("id".into()),
                Tok::Comma,
                Tok::QuotedIdent("my col".into()),
                Tok::Ident("FROM".into()),
                Tok::Ident("t".into()),
                Tok::Ident("WHERE".into()),
                Tok::Ident("a".into()),
                Tok::Le,
                Tok::Number("1.5e3".into()),
                Tok::Ident("AND".into()),
                Tok::Ident("b".into()),
                Tok::Ne,
                Tok::Str("x'y".into()),
                Tok::Semicolon,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn comments_and_escapes() {
        assert_eq!(
            toks("-- c1\n# c2\n/* c3 */ 'a\\nb' \"q\" .5 1e x"),
            vec![
                Tok::Str("a\nb".into()),
                Tok::Str("q".into()),
                Tok::Number(".5".into()),
                Tok::Number("1".into()),
                Tok::Ident("e".into()),
                Tok::Ident("x".into()),
                Tok::Eof,
            ]
        );
        assert_eq!(toks("1--2"), vec![Tok::Number("1".into()), Tok::Minus, Tok::Minus, Tok::Number("2".into()), Tok::Eof]);
        assert_eq!(toks("'旅行攻略'"), vec![Tok::Str("旅行攻略".into()), Tok::Eof]);
    }

    #[test]
    fn lex_errors() {
        assert!(tokenize("'abc").is_err());
        assert!(tokenize("/* x").is_err());
        assert!(tokenize("a @ b").is_err());
        assert!(tokenize("``").is_err());
    }
}
