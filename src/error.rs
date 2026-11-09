use std::fmt;

/// Error categories. The protocol layer maps these to the official error
/// codes once the problem package defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Unsupported,
    TableExists,
    NoSuchTable,
    NoSuchColumn,
    DuplicateKey,
    /// INSERT row has a different number of values than target columns.
    ColumnCount,
    /// INSERT column list leaves out a column (there are no defaults).
    NoDefault,
    TypeMismatch,
    OutOfRange,
    DataTooLong,
    VectorDim,
    InvalidValue,
    Schema,
    TxnState,
    Io,
    Corrupt,
    Internal,
}

#[derive(Debug, Clone)]
pub struct DbError {
    pub kind: ErrorKind,
    pub msg: String,
}

impl DbError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        DbError { kind, msg: msg.into() }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.msg)
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::new(ErrorKind::Io, e.to_string())
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Builds a `DbError` of the given kind with a formatted message.
#[macro_export]
macro_rules! db_err {
    ($kind:ident, $($arg:tt)*) => {
        $crate::error::DbError::new($crate::error::ErrorKind::$kind, format!($($arg)*))
    };
}

/// Returns early with a `DbError` of the given kind.
#[macro_export]
macro_rules! bail {
    ($kind:ident, $($arg:tt)*) => {
        return Err($crate::db_err!($kind, $($arg)*))
    };
}
