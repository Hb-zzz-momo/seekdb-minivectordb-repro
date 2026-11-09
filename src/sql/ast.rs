use crate::types::DataType;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable { name: String, if_exists: bool },
    CreateVectorIndex(CreateVectorIndex),
    Insert(Insert),
    Select(Select),
    Update(Update),
    Delete(Delete),
    Begin,
    Commit,
    Rollback,
    /// `SET name = value`, e.g. a search-mode switch if the driver uses one.
    Set { name: String, value: Expr },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    /// Table-level `PRIMARY KEY (...)` columns, if given.
    pub primary_key: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateVectorIndex {
    pub name: String,
    pub table: String,
    pub column: String,
    /// `WITH (key = value, ...)` options, lower-cased keys, raw values.
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Star,
    Expr {
        expr: Expr,
        alias: Option<String>,
        /// Source text of the expression, used as the result column name.
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub items: Vec<SelectItem>,
    pub table: String,
    pub table_alias: Option<String>,
    pub filter: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: String,
    pub sets: Vec<(String, Expr)>,
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    /// Decimal, exponent or out-of-range integer literal, kept as source text.
    Num(String),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column { table: Option<String>, name: String },
    Lit(Literal),
    /// Function call; `name` is upper-cased.
    Func { name: String, args: Vec<Expr> },
    Cmp { op: CmpOp, left: Box<Expr>, right: Box<Expr> },
    And(Box<Expr>, Box<Expr>),
}
