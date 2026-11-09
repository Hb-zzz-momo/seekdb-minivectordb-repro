//! Recursive-descent parser for the MiniVectorDB SQL subset. Constructs that
//! are valid SQL but outside the subset (OR, JOIN, NULL, ...) fail with
//! `Unsupported` rather than `Syntax`, so they are never silently reinterpreted.

use super::ast::*;
use super::lexer::{tokenize, Tok, Token};
use crate::error::{DbError, DbResult};
use crate::types::DataType;
use crate::{bail, db_err};

/// Words that cannot be used as unquoted identifiers.
const RESERVED: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "AS", "ASC", "BETWEEN", "BY", "CREATE", "DELETE", "DESC", "DISTINCT", "DROP",
    "EXISTS", "FROM", "GROUP", "HAVING", "IN", "INDEX", "INSERT", "INTO", "IS", "JOIN", "KEY", "LIKE", "LIMIT",
    "NOT", "NULL", "ON", "OR", "ORDER", "PRIMARY", "SELECT", "SET", "TABLE", "UNION", "UPDATE", "VALUES", "WHERE",
    "XOR",
];

/// Statement verbs we recognise but do not implement.
const UNSUPPORTED_VERBS: &[&str] = &[
    "ALTER", "ANALYZE", "CALL", "DESC", "DESCRIBE", "DO", "EXPLAIN", "GRANT", "HANDLER", "LOAD", "LOCK", "OPTIMIZE",
    "RELEASE", "RENAME", "REPLACE", "REVOKE", "SAVEPOINT", "SHOW", "TRUNCATE", "UNLOCK", "USE", "WITH", "XA",
];

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

/// Parses one statement; a trailing `;` is allowed.
pub fn parse(sql: &str) -> DbResult<Statement> {
    let toks = tokenize(sql)?;
    if toks[0].tok == Tok::Eof {
        bail!(Syntax, "empty statement");
    }
    let mut p = Parser { src: sql, toks, pos: 0 };
    let stmt = p.statement()?;
    while p.eat(&Tok::Semicolon) {}
    if !p.at(&Tok::Eof) {
        return Err(p.unexpected());
    }
    Ok(stmt)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek2(&self) -> &Tok {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)].tok
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), Tok::Semicolon | Tok::Eof)
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok) -> DbResult<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn is_any_kw(&self, kws: &[&str]) -> bool {
        kws.iter().any(|k| self.is_kw(k))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> DbResult<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn unexpected(&self) -> DbError {
        let t = &self.toks[self.pos];
        if t.tok == Tok::Eof {
            db_err!(Syntax, "unexpected end of statement")
        } else {
            db_err!(Syntax, "syntax error near '{}'", &self.src[t.start..])
        }
    }

    fn unsupported(&self, what: &str) -> DbError {
        db_err!(Unsupported, "{} is not supported", what)
    }

    fn ident(&mut self) -> DbResult<String> {
        match self.peek() {
            Tok::Ident(s) if !is_reserved(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            Tok::QuotedIdent(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(self.unexpected()),
        }
    }

    /// Identifier optionally qualified as `table.column`; the qualifier is
    /// dropped (single-table statements only).
    fn column_name(&mut self) -> DbResult<String> {
        let first = self.ident()?;
        if self.eat(&Tok::Dot) {
            return self.ident();
        }
        Ok(first)
    }

    fn unsigned(&mut self) -> DbResult<u64> {
        match self.peek() {
            Tok::Number(n) => {
                let v = n.parse().map_err(|_| db_err!(Syntax, "expected an unsigned integer, got '{}'", n))?;
                self.advance();
                Ok(v)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn length(&mut self) -> DbResult<u32> {
        self.expect(&Tok::LParen)?;
        let n = self.unsigned()?;
        self.expect(&Tok::RParen)?;
        u32::try_from(n).map_err(|_| db_err!(Schema, "length {} is too large", n))
    }

    fn statement(&mut self) -> DbResult<Statement> {
        let verb = match self.peek() {
            Tok::Ident(s) => s.to_ascii_uppercase(),
            Tok::LParen => return Err(self.unsupported("parenthesized query")),
            _ => return Err(self.unexpected()),
        };
        match verb.as_str() {
            "CREATE" => self.create(),
            "DROP" => self.drop_table(),
            "INSERT" => self.insert(),
            "SELECT" => self.select().map(Statement::Select),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "BEGIN" => {
                self.advance();
                self.eat_kw("WORK");
                Ok(Statement::Begin)
            }
            "START" => {
                self.advance();
                self.expect_kw("TRANSACTION")?;
                if !self.at_end() {
                    return Err(self.unsupported("transaction characteristics"));
                }
                Ok(Statement::Begin)
            }
            "COMMIT" => {
                self.advance();
                self.eat_kw("WORK");
                Ok(Statement::Commit)
            }
            "ROLLBACK" => {
                self.advance();
                self.eat_kw("WORK");
                if self.is_kw("TO") {
                    return Err(self.unsupported("ROLLBACK TO SAVEPOINT"));
                }
                Ok(Statement::Rollback)
            }
            "SET" => self.set(),
            v if UNSUPPORTED_VERBS.contains(&v) => Err(db_err!(Unsupported, "{} statement is not supported", v)),
            _ => Err(self.unexpected()),
        }
    }

    fn create(&mut self) -> DbResult<Statement> {
        self.advance();
        if self.eat_kw("TABLE") {
            return self.create_table();
        }
        if self.eat_kw("VECTOR") {
            self.expect_kw("INDEX")?;
            return self.create_vector_index();
        }
        if self.is_any_kw(&["INDEX", "UNIQUE", "FULLTEXT", "SPATIAL"]) {
            return Err(self.unsupported("CREATE INDEX"));
        }
        if self.is_any_kw(&["TEMPORARY", "DATABASE", "SCHEMA", "VIEW", "TRIGGER", "PROCEDURE", "FUNCTION", "OR"]) {
            return Err(self.unsupported("this CREATE statement"));
        }
        Err(self.unexpected())
    }

    fn create_table(&mut self) -> DbResult<Statement> {
        let if_not_exists = if self.eat_kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        if self.is_kw("LIKE") || self.is_kw("AS") || self.is_kw("SELECT") {
            return Err(self.unsupported("CREATE TABLE ... LIKE/AS"));
        }
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        let mut primary_key = None;
        loop {
            if self.eat_kw("PRIMARY") {
                self.expect_kw("KEY")?;
                self.expect(&Tok::LParen)?;
                let mut cols = vec![self.ident()?];
                while self.eat(&Tok::Comma) {
                    cols.push(self.ident()?);
                }
                self.expect(&Tok::RParen)?;
                if primary_key.is_some() {
                    bail!(Schema, "multiple primary keys defined");
                }
                primary_key = Some(cols);
            } else if (self.is_kw("VECTOR") && self.peek2_is_kw("INDEX"))
                || self.is_any_kw(&["KEY", "INDEX", "UNIQUE", "CONSTRAINT", "FOREIGN", "CHECK", "FULLTEXT"])
            {
                return Err(self.unsupported("table constraints and inline indexes"));
            } else {
                columns.push(self.column_def()?);
            }
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen)?;
            break;
        }
        if !self.at_end() {
            return Err(self.unsupported("table options"));
        }
        Ok(Statement::CreateTable(CreateTable { name, if_not_exists, columns, primary_key }))
    }

    fn peek2_is_kw(&self, kw: &str) -> bool {
        matches!(self.peek2(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn column_def(&mut self) -> DbResult<ColumnDef> {
        let name = self.ident()?;
        let ty = self.data_type()?;
        let mut primary_key = false;
        loop {
            if self.eat_kw("PRIMARY") {
                self.expect_kw("KEY")?;
                primary_key = true;
            } else if self.eat_kw("KEY") {
                primary_key = true;
            } else if self.eat_kw("NOT") {
                self.expect_kw("NULL")?;
            } else if matches!(self.peek(), Tok::Comma | Tok::RParen) {
                break;
            } else if let Tok::Ident(s) = self.peek() {
                bail!(Unsupported, "column attribute {} is not supported", s.to_ascii_uppercase());
            } else {
                return Err(self.unexpected());
            }
        }
        Ok(ColumnDef { name, ty, primary_key })
    }

    fn data_type(&mut self) -> DbResult<DataType> {
        let name = match self.peek() {
            Tok::Ident(s) => s.to_ascii_uppercase(),
            _ => return Err(self.unexpected()),
        };
        self.advance();
        let ty = match name.as_str() {
            "BIGINT" => {
                if self.at(&Tok::LParen) {
                    self.length()?; // display width, no semantic effect
                }
                DataType::BigInt
            }
            "VARCHAR" => DataType::Varchar(self.length()?),
            "VECTOR" => {
                let n = self.length()?;
                if n == 0 {
                    bail!(Schema, "vector dimension must be positive");
                }
                DataType::Vector(n)
            }
            _ => bail!(Unsupported, "data type {} is not supported", name),
        };
        if self.is_any_kw(&["UNSIGNED", "SIGNED", "ZEROFILL"]) {
            return Err(self.unsupported("integer sign attributes"));
        }
        Ok(ty)
    }

    fn create_vector_index(&mut self) -> DbResult<Statement> {
        let name = self.ident()?;
        self.expect_kw("ON")?;
        let table = self.ident()?;
        self.expect(&Tok::LParen)?;
        let column = self.ident()?;
        if self.at(&Tok::Comma) {
            return Err(self.unsupported("multi-column vector index"));
        }
        self.expect(&Tok::RParen)?;
        let mut options = Vec::new();
        if self.eat_kw("WITH") {
            self.expect(&Tok::LParen)?;
            loop {
                let key = self.ident()?.to_ascii_lowercase();
                self.expect(&Tok::Eq)?;
                let value = match self.peek() {
                    Tok::Ident(s) | Tok::QuotedIdent(s) | Tok::Number(s) | Tok::Str(s) => s.clone(),
                    _ => return Err(self.unexpected()),
                };
                self.advance();
                options.push((key, value));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
        }
        if !self.at_end() {
            return Err(self.unsupported("vector index options"));
        }
        Ok(Statement::CreateVectorIndex(CreateVectorIndex { name, table, column, options }))
    }

    fn drop_table(&mut self) -> DbResult<Statement> {
        self.advance();
        if !self.eat_kw("TABLE") {
            if self.is_kw("INDEX") {
                return Err(self.unsupported("DROP INDEX"));
            }
            return Err(self.unsupported("this DROP statement"));
        }
        let if_exists = if self.eat_kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        if self.at(&Tok::Comma) {
            return Err(self.unsupported("dropping several tables at once"));
        }
        Ok(Statement::DropTable { name, if_exists })
    }

    fn insert(&mut self) -> DbResult<Statement> {
        self.advance();
        if self.is_any_kw(&["IGNORE", "LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY"]) {
            return Err(self.unsupported("INSERT modifiers"));
        }
        self.eat_kw("INTO");
        let table = self.ident()?;
        let columns = if self.eat(&Tok::LParen) {
            let mut cols = vec![self.column_name()?];
            while self.eat(&Tok::Comma) {
                cols.push(self.column_name()?);
            }
            self.expect(&Tok::RParen)?;
            Some(cols)
        } else {
            None
        };
        if !(self.eat_kw("VALUES") || self.eat_kw("VALUE")) {
            if self.is_kw("SET") || self.is_kw("SELECT") {
                return Err(self.unsupported("INSERT ... SET/SELECT"));
            }
            return Err(self.unexpected());
        }
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen)?;
            let mut row = Vec::new();
            if !self.at(&Tok::RParen) {
                loop {
                    row.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RParen)?;
            rows.push(row);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.is_kw("ON") {
            return Err(self.unsupported("ON DUPLICATE KEY UPDATE"));
        }
        Ok(Statement::Insert(Insert { table, columns, rows }))
    }

    fn select(&mut self) -> DbResult<Select> {
        self.advance();
        if self.is_any_kw(&["DISTINCT", "DISTINCTROW", "SQL_CALC_FOUND_ROWS"]) {
            return Err(self.unsupported("DISTINCT"));
        }
        self.eat_kw("ALL");
        let mut items = Vec::new();
        loop {
            if self.eat(&Tok::Star) {
                items.push(SelectItem::Star);
            } else {
                let start = self.toks[self.pos].start;
                let expr = self.expr()?;
                let end = self.toks[self.pos - 1].end;
                let text = self.src[start..end].to_string();
                let alias = self.alias()?;
                items.push(SelectItem::Expr { expr, alias, text });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if !self.eat_kw("FROM") {
            if self.at_end() {
                return Err(self.unsupported("SELECT without FROM"));
            }
            return Err(self.unexpected());
        }
        let table = self.ident()?;
        let table_alias = self.alias()?;
        if self.at(&Tok::Comma)
            || self.is_any_kw(&["JOIN", "INNER", "LEFT", "RIGHT", "CROSS", "NATURAL", "STRAIGHT_JOIN", "FULL"])
        {
            return Err(self.unsupported("JOIN"));
        }
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        if self.is_kw("GROUP") || self.is_kw("HAVING") {
            return Err(self.unsupported("GROUP BY/HAVING"));
        }
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let expr = self.expr()?;
                let desc = if self.eat_kw("DESC") {
                    true
                } else {
                    self.eat_kw("ASC");
                    false
                };
                order_by.push(OrderItem { expr, desc });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        if self.eat_kw("LIMIT") {
            let first = self.unsigned()?;
            if self.eat(&Tok::Comma) {
                offset = Some(first);
                limit = Some(self.unsigned()?);
            } else {
                limit = Some(first);
                if self.eat_kw("OFFSET") {
                    offset = Some(self.unsigned()?);
                }
            }
        }
        if self.is_any_kw(&["UNION", "FOR", "LOCK", "INTO"]) {
            return Err(self.unsupported("this SELECT clause"));
        }
        Ok(Select { items, table, table_alias, filter, order_by, limit, offset })
    }

    /// `[AS] alias` after a select item or table name.
    fn alias(&mut self) -> DbResult<Option<String>> {
        if self.eat_kw("AS") {
            return match self.peek() {
                Tok::Str(s) => {
                    let s = s.clone();
                    self.advance();
                    Ok(Some(s))
                }
                _ => self.ident().map(Some),
            };
        }
        match self.peek() {
            Tok::Ident(s) if !is_reserved(s) && !s.eq_ignore_ascii_case("OFFSET") => self.ident().map(Some),
            Tok::QuotedIdent(_) => self.ident().map(Some),
            _ => Ok(None),
        }
    }

    fn update(&mut self) -> DbResult<Statement> {
        self.advance();
        if self.is_any_kw(&["LOW_PRIORITY", "IGNORE"]) {
            return Err(self.unsupported("UPDATE modifiers"));
        }
        let table = self.ident()?;
        if self.at(&Tok::Comma) || self.is_kw("JOIN") {
            return Err(self.unsupported("multi-table UPDATE"));
        }
        self.expect_kw("SET")?;
        let mut sets = Vec::new();
        loop {
            let col = self.column_name()?;
            self.expect(&Tok::Eq)?;
            sets.push((col, self.expr()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        if self.is_kw("ORDER") || self.is_kw("LIMIT") {
            return Err(self.unsupported("UPDATE with ORDER BY/LIMIT"));
        }
        Ok(Statement::Update(Update { table, sets, filter }))
    }

    fn delete(&mut self) -> DbResult<Statement> {
        self.advance();
        if self.is_any_kw(&["LOW_PRIORITY", "QUICK", "IGNORE"]) {
            return Err(self.unsupported("DELETE modifiers"));
        }
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        if self.at(&Tok::Comma) || self.is_kw("USING") || self.is_kw("JOIN") {
            return Err(self.unsupported("multi-table DELETE"));
        }
        let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        if self.is_kw("ORDER") || self.is_kw("LIMIT") {
            return Err(self.unsupported("DELETE with ORDER BY/LIMIT"));
        }
        Ok(Statement::Delete(Delete { table, filter }))
    }

    fn set(&mut self) -> DbResult<Statement> {
        self.advance();
        if !self.eat_kw("SESSION") {
            self.eat_kw("GLOBAL");
        }
        if self.is_any_kw(&["TRANSACTION", "NAMES", "CHARACTER", "PASSWORD"]) {
            return Err(self.unsupported("this SET statement"));
        }
        let name = self.ident()?;
        self.expect(&Tok::Eq)?;
        let value = self.expr()?;
        if self.at(&Tok::Comma) {
            return Err(self.unsupported("setting several variables at once"));
        }
        Ok(Statement::Set { name, value })
    }

    fn expr(&mut self) -> DbResult<Expr> {
        let mut left = self.comparison()?;
        loop {
            if self.eat_kw("AND") {
                let right = self.comparison()?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else if self.is_any_kw(&["OR", "XOR"]) {
                return Err(self.unsupported("OR"));
            } else {
                return Ok(left);
            }
        }
    }

    fn comparison(&mut self) -> DbResult<Expr> {
        if self.is_kw("NOT") {
            return Err(self.unsupported("NOT"));
        }
        let left = self.unary()?;
        let Some(op) = self.cmp_op()? else {
            return Ok(left);
        };
        self.advance();
        let right = self.unary()?;
        if self.cmp_op()?.is_some() {
            return Err(self.unsupported("chained comparisons"));
        }
        Ok(Expr::Cmp { op, left: Box::new(left), right: Box::new(right) })
    }

    /// Comparison operator at the cursor, or an error for operators outside
    /// the subset.
    fn cmp_op(&self) -> DbResult<Option<CmpOp>> {
        Ok(Some(match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash | Tok::Percent => {
                return Err(self.unsupported("arithmetic expressions"))
            }
            _ if self.is_any_kw(&["IS", "IN", "BETWEEN", "LIKE", "NOT", "REGEXP", "RLIKE", "DIV", "MOD"]) => {
                let Tok::Ident(s) = self.peek() else { unreachable!() };
                bail!(Unsupported, "{} is not supported", s.to_ascii_uppercase());
            }
            _ => return Ok(None),
        }))
    }

    fn unary(&mut self) -> DbResult<Expr> {
        if self.at(&Tok::Minus) || self.at(&Tok::Plus) {
            let negative = self.at(&Tok::Minus);
            self.advance();
            if let Tok::Number(n) = self.peek() {
                let text = if negative { format!("-{n}") } else { n.clone() };
                self.advance();
                return Ok(Expr::Lit(number_literal(text)));
            }
            return Err(self.unsupported("arithmetic expressions"));
        }
        self.primary()
    }

    fn primary(&mut self) -> DbResult<Expr> {
        match self.peek() {
            Tok::Number(n) => {
                let lit = number_literal(n.clone());
                self.advance();
                Ok(Expr::Lit(lit))
            }
            Tok::Str(s) => {
                let lit = Literal::Str(s.clone());
                self.advance();
                Ok(Expr::Lit(lit))
            }
            Tok::LParen => {
                self.advance();
                if self.is_kw("SELECT") {
                    return Err(self.unsupported("subqueries"));
                }
                let e = self.expr()?;
                if self.at(&Tok::Comma) {
                    return Err(self.unsupported("row constructors"));
                }
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("NULL") => Err(self.unsupported("NULL")),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TRUE") || s.eq_ignore_ascii_case("FALSE") => {
                Err(self.unsupported("boolean literals"))
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("EXISTS") => Err(self.unsupported("subqueries")),
            Tok::Ident(s) if *self.peek2() == Tok::LParen => {
                let name = s.to_ascii_uppercase();
                self.advance();
                self.advance();
                let mut args = Vec::new();
                if !self.at(&Tok::RParen) {
                    loop {
                        if self.at(&Tok::Star) {
                            return Err(self.unsupported("aggregate functions"));
                        }
                        args.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                Ok(Expr::Func { name, args })
            }
            Tok::Ident(_) | Tok::QuotedIdent(_) => {
                let first = self.ident()?;
                if self.eat(&Tok::Dot) {
                    let name = self.ident()?;
                    return Ok(Expr::Column { table: Some(first), name });
                }
                Ok(Expr::Column { table: None, name: first })
            }
            _ => Err(self.unexpected()),
        }
    }
}

fn number_literal(text: String) -> Literal {
    if !text.contains(['.', 'e', 'E']) {
        if let Ok(v) = text.parse::<i64>() {
            return Literal::Int(v);
        }
    }
    Literal::Num(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn col(name: &str) -> Expr {
        Expr::Column { table: None, name: name.into() }
    }

    fn kind(sql: &str) -> ErrorKind {
        parse(sql).unwrap_err().kind
    }

    #[test]
    fn create_table() {
        let s = parse(
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR(32), content VARCHAR(1024), embedding VECTOR(4));",
        )
        .unwrap();
        let Statement::CreateTable(ct) = s else { panic!() };
        assert_eq!(ct.name, "docs");
        assert_eq!(ct.columns.len(), 4);
        assert!(ct.columns[0].primary_key);
        assert_eq!(ct.columns[1].ty, DataType::Varchar(32));
        assert_eq!(ct.columns[3].ty, DataType::Vector(4));

        let Statement::CreateTable(ct) =
            parse("create table if not exists t (a bigint not null, v vector(2), primary key (a))").unwrap()
        else {
            panic!()
        };
        assert!(ct.if_not_exists);
        assert_eq!(ct.primary_key, Some(vec!["a".to_string()]));

        assert_eq!(kind("CREATE TABLE t (a INT PRIMARY KEY)"), ErrorKind::Unsupported);
        assert_eq!(kind("CREATE TABLE t (a BIGINT DEFAULT 1)"), ErrorKind::Unsupported);
        assert_eq!(kind("CREATE TABLE t (a BIGINT) ENGINE=InnoDB"), ErrorKind::Unsupported);
        assert_eq!(kind("CREATE TABLE t (a BIGINT NULL)"), ErrorKind::Unsupported);
        assert_eq!(kind("CREATE TABLE t (v VECTOR(0))"), ErrorKind::Schema);
        assert_eq!(kind("CREATE TABLE t (a BIGINT"), ErrorKind::Syntax);
    }

    #[test]
    fn column_named_vector() {
        let Statement::CreateTable(ct) = parse("CREATE TABLE t (id BIGINT PRIMARY KEY, vector VECTOR(3))").unwrap() else {
            panic!()
        };
        assert_eq!(ct.columns[1].name, "vector");
    }

    #[test]
    fn insert_multi_row() {
        let s = parse(
            "INSERT INTO docs VALUES (1, 'database', '事务与恢复', '[1, 0, 0, 0]'), (2, 'x', 'y', '[0,1,0,0]'), (-9223372036854775808, 'a', 'b', '[0,0,0,1]');",
        )
        .unwrap();
        let Statement::Insert(ins) = s else { panic!() };
        assert_eq!(ins.rows.len(), 3);
        assert_eq!(ins.rows[0][2], Expr::Lit(Literal::Str("事务与恢复".into())));
        assert_eq!(ins.rows[2][0], Expr::Lit(Literal::Int(i64::MIN)));

        let Statement::Insert(ins) = parse("INSERT INTO t (id, v) VALUES (1.5, '[1]'), (99999999999999999999, '[2]')").unwrap()
        else {
            panic!()
        };
        assert_eq!(ins.columns, Some(vec!["id".to_string(), "v".to_string()]));
        assert_eq!(ins.rows[0][0], Expr::Lit(Literal::Num("1.5".into())));
        assert_eq!(ins.rows[1][0], Expr::Lit(Literal::Num("99999999999999999999".into())));
        assert_eq!(kind("INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1"), ErrorKind::Unsupported);
        assert_eq!(kind("INSERT INTO t VALUES (NULL)"), ErrorKind::Unsupported);
        assert_eq!(kind("INSERT INTO t VALUES (1 + 2)"), ErrorKind::Unsupported);
    }

    #[test]
    fn select_topk() {
        let s = parse(
            "SELECT id, content,\n       L2_DISTANCE(embedding, '[0.9, 0.1, 0, 0]') AS distance\nFROM docs\nWHERE category = 'database'\nORDER BY distance\nLIMIT 2;",
        )
        .unwrap();
        let Statement::Select(sel) = s else { panic!() };
        assert_eq!(sel.items.len(), 3);
        let SelectItem::Expr { expr, alias, text } = &sel.items[2] else { panic!() };
        assert_eq!(alias.as_deref(), Some("distance"));
        assert_eq!(text, "L2_DISTANCE(embedding, '[0.9, 0.1, 0, 0]')");
        assert_eq!(
            *expr,
            Expr::Func {
                name: "L2_DISTANCE".into(),
                args: vec![col("embedding"), Expr::Lit(Literal::Str("[0.9, 0.1, 0, 0]".into()))]
            }
        );
        assert_eq!(
            sel.filter,
            Some(Expr::Cmp {
                op: CmpOp::Eq,
                left: Box::new(col("category")),
                right: Box::new(Expr::Lit(Literal::Str("database".into())))
            })
        );
        assert_eq!(sel.order_by, vec![OrderItem { expr: col("distance"), desc: false }]);
        assert_eq!(sel.limit, Some(2));
    }

    #[test]
    fn select_clauses() {
        let Statement::Select(sel) =
            parse("select * from t where a >= -3 and b < 'x' and (c <> 2) order by a desc, b limit 5, 10").unwrap()
        else {
            panic!()
        };
        assert_eq!(sel.items, vec![SelectItem::Star]);
        assert_eq!((sel.offset, sel.limit), (Some(5), Some(10)));
        assert_eq!(sel.order_by.len(), 2);
        assert!(sel.order_by[0].desc);

        let Statement::Select(sel) = parse("SELECT d.id x FROM docs d LIMIT 3 OFFSET 1").unwrap() else { panic!() };
        assert_eq!(sel.table_alias.as_deref(), Some("d"));
        assert_eq!((sel.offset, sel.limit), (Some(1), Some(3)));
        let SelectItem::Expr { alias, .. } = &sel.items[0] else { panic!() };
        assert_eq!(alias.as_deref(), Some("x"));

        assert_eq!(kind("SELECT * FROM t WHERE a = 1 OR b = 2"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT * FROM t JOIN u"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT * FROM t, u"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT COUNT(*) FROM t"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t GROUP BY a"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t WHERE a IN (1, 2)"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t WHERE a IS NULL"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t WHERE a = (SELECT 1)"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT 1"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t WHERE NOT a = 1"), ErrorKind::Unsupported);
        assert_eq!(kind("SELECT a FROM t LIMIT -1"), ErrorKind::Syntax);
        assert_eq!(kind("SELECT FROM t"), ErrorKind::Syntax);
        assert_eq!(kind("SELEC * FROM t"), ErrorKind::Syntax);
    }

    #[test]
    fn dml_and_txn() {
        let Statement::Update(u) = parse("UPDATE docs SET embedding = '[0.8, 0.2, 0, 0]' WHERE id = 1;").unwrap() else {
            panic!()
        };
        assert_eq!(u.sets.len(), 1);
        assert!(u.filter.is_some());
        let Statement::Delete(d) = parse("DELETE FROM docs WHERE id = 2").unwrap() else { panic!() };
        assert_eq!(d.table, "docs");
        assert_eq!(parse("BEGIN;").unwrap(), Statement::Begin);
        assert_eq!(parse("start transaction").unwrap(), Statement::Begin);
        assert_eq!(parse("COMMIT").unwrap(), Statement::Commit);
        assert_eq!(parse("rollback work;").unwrap(), Statement::Rollback);
        assert_eq!(kind("ROLLBACK TO SAVEPOINT s1"), ErrorKind::Unsupported);
        assert_eq!(kind("ALTER TABLE t ADD COLUMN c BIGINT"), ErrorKind::Unsupported);
        assert_eq!(kind("TRUNCATE TABLE t"), ErrorKind::Unsupported);
        assert_eq!(kind("DELETE FROM t LIMIT 1"), ErrorKind::Unsupported);
        assert_eq!(kind("   -- only a comment"), ErrorKind::Syntax);
    }

    #[test]
    fn vector_index_and_set() {
        let Statement::CreateVectorIndex(ix) = parse("CREATE VECTOR INDEX idx ON docs(embedding)").unwrap() else {
            panic!()
        };
        assert_eq!((ix.name.as_str(), ix.table.as_str(), ix.column.as_str()), ("idx", "docs", "embedding"));
        let Statement::CreateVectorIndex(ix) =
            parse("CREATE VECTOR INDEX i2 ON t(v) WITH (distance=l2, type=hnsw, m=16)").unwrap()
        else {
            panic!()
        };
        assert_eq!(ix.options.len(), 3);
        assert_eq!(kind("CREATE INDEX i ON t(a)"), ErrorKind::Unsupported);
        assert_eq!(
            parse("SET vector_search_mode = 'ann'").unwrap(),
            Statement::Set { name: "vector_search_mode".into(), value: Expr::Lit(Literal::Str("ann".into())) }
        );
        assert_eq!(
            parse("DROP TABLE IF EXISTS docs").unwrap(),
            Statement::DropTable { name: "docs".into(), if_exists: true }
        );
    }
}
